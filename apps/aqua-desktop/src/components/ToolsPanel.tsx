import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshCw, Shield, Wrench, ScrollText, Square } from 'lucide-react';
import { toolsApi, type AuditRecord, type Decision, type Limits, type Mode, type PolicyFile, type ToolInfo, type ToolStatus } from '../api/tools';

type Tab = 'overview' | 'policy' | 'activity' | 'tools';

const MODES: { id: Mode; title: string; text: string }[] = [
  { id: 'locked', title: 'Locked', text: 'Reads ask first; everything else is refused. For demos and unattended machines.' },
  { id: 'standard', title: 'Standard', text: 'Reads run; changes ask unless they are inside a trusted, allowed target. The default.' },
  { id: 'developer', title: 'Developer', text: 'Routine local work runs without asking. Destructive, credential, remote, execute and external-effect calls still ask.' },
];

const DECISIONS: Decision[] = ['auto', 'ask', 'always_ask', 'deny'];



function domainOf(name: string): string {
  return name.split('.')[0] ?? name;
}

export function ToolsPanel() {
  const [tab, setTab] = useState<Tab>('overview');
  const [status, setStatus] = useState<ToolStatus | null>(null);
  const [tools, setTools] = useState<ToolInfo[]>([]);
  const [policy, setPolicy] = useState<PolicyFile | null>(null);
  const [draft, setDraft] = useState<PolicyFile | null>(null);
  const [records, setRecords] = useState<AuditRecord[]>([]);
  const [verify, setVerify] = useState<string>('');
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      const [s, t, p, r] = await Promise.all([toolsApi.status(), toolsApi.list(), toolsApi.policy(), toolsApi.recent(60)]);
      setStatus(s);
      setTools(t.tools);
      setPolicy(p);
      setDraft((d) => d ?? p);
      setRecords(r.records);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not reach Aqua');
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // Activity refreshes itself while it is open.
  useEffect(() => {
    if (tab !== 'activity') return;
    const id = window.setInterval(() => void toolsApi.recent(60).then((r) => setRecords(r.records)).catch(() => undefined), 4000);
    return () => window.clearInterval(id);
  }, [tab]);

  const dirty = useMemo(() => JSON.stringify(draft) !== JSON.stringify(policy), [draft, policy]);

  const save = async () => {
    if (!draft) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await toolsApi.savePolicy(draft);
      setPolicy(saved);
      setDraft(saved);
      setNotice('Policy saved. It applies to the next tool call and is kept after a restart.');
      void load();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the policy');
    } finally {
      setBusy(false);
    }
  };

  const groups = useMemo(() => {
    const m = new Map<string, ToolInfo[]>();
    for (const t of tools) m.set(domainOf(t.name), [...(m.get(domainOf(t.name)) ?? []), t]);
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }, [tools]);

  const tabBtn = (id: Tab, text: string, Icon: typeof Shield) => (
    <button
      key={id}
      onClick={() => setTab(id)}
      style={{
        display: 'flex', alignItems: 'center', gap: 8, padding: '8px 14px', borderRadius: 8, border: '1px solid var(--glass-border)', cursor: 'pointer',
        background: tab === id ? 'rgba(6,182,212,0.16)' : 'transparent', color: tab === id ? 'var(--accent-cyan)' : 'var(--text-secondary)', fontWeight: tab === id ? 600 : 400,
      }}
    >
      <Icon size={15} /> {text}
    </button>
  );

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '24px', maxWidth: '1200px', margin: '0 auto', width: '100%', animation: 'fade-in 0.3s ease-out' }}>
      
      <div className="glass-panel" style={{ padding: '24px', borderRadius: '16px', position: 'relative', overflow: 'hidden' }}>
        <div style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '4px', background: 'linear-gradient(90deg, var(--accent-cyan), var(--accent-emerald))' }} />
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '16px', flexWrap: 'wrap' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '16px' }}>
            <div style={{ width: '48px', height: '48px', borderRadius: '12px', background: 'rgba(0, 240, 255, 0.1)', display: 'flex', alignItems: 'center', justifyContent: 'center', border: '1px solid rgba(0, 240, 255, 0.2)', boxShadow: '0 0 15px rgba(0, 240, 255, 0.15)' }}>
              <Shield size={24} color="var(--accent-cyan)" />
            </div>
            <div>
              <h2 style={{ margin: 0, fontSize: '24px', fontWeight: '700', letterSpacing: '-0.02em', color: 'var(--text-primary)' }}>Tools & Approvals</h2>
              <p style={{ margin: '4px 0 0 0', fontSize: '14px', color: 'var(--text-secondary)' }}>Manage permissions, policies, and audit trails for agent tool usage.</p>
            </div>
          </div>

          <div style={{ display: 'flex', gap: '12px' }}>
            <button 
              onClick={() => void load()} 
              style={{
                padding: '10px 16px',
                borderRadius: '8px',
                border: '1px solid var(--glass-border)',
                background: 'rgba(255,255,255,0.05)',
                color: 'var(--text-primary)',
                fontWeight: '600',
                cursor: 'pointer',
                display: 'flex',
                alignItems: 'center',
                gap: '8px',
                fontSize: '13px',
                transition: 'all 0.2s'
              }}
            >
              <RefreshCw size={14} /> Refresh
            </button>
            <button
              onClick={() => void toolsApi.cancelAgent().then(() => setNotice('Stop requested: agent runs end before their next step and running tool calls are cancelled.')).catch((e) => setError(String(e)))}
              style={{
                padding: '10px 16px',
                borderRadius: '8px',
                border: '1px solid rgba(248, 113, 113, 0.4)',
                background: 'rgba(248, 113, 113, 0.15)',
                color: '#fca5a5',
                fontWeight: '600',
                cursor: 'pointer',
                display: 'flex',
                alignItems: 'center',
                gap: '8px',
                fontSize: '13px',
                boxShadow: '0 0 10px rgba(248, 113, 113, 0.2)',
                transition: 'all 0.2s'
              }}
              title="Stop every agent run and cancel tool calls in flight"
            >
              <Square size={14} /> Stop Agent
            </button>
          </div>
        </div>
      </div>

      <div style={{ display: 'flex', gap: '8px', flexWrap: 'wrap' }}>
        {tabBtn('overview', 'Overview', Shield)}
        {tabBtn('policy', 'Policy', Shield)}
        {tabBtn('activity', 'Activity', ScrollText)}
        {tabBtn('tools', 'Tools', Wrench)}
      </div>

      {error && (
        <div className="glass-panel" style={{ padding: '16px', borderRadius: '12px', background: 'rgba(239, 68, 68, 0.05)', border: '1px solid rgba(239, 68, 68, 0.3)', color: 'var(--accent-rose)', display: 'flex', alignItems: 'center', gap: '10px', fontSize: '14px', animation: 'slide-down 0.2s ease-out' }}>
          <span>{error}</span>
        </div>
      )}
      
      {notice && (
        <div className="glass-panel" style={{ padding: '16px', borderRadius: '12px', background: 'rgba(16, 185, 129, 0.05)', border: '1px solid rgba(16, 185, 129, 0.3)', color: 'var(--accent-emerald)', display: 'flex', alignItems: 'center', gap: '10px', fontSize: '14px', animation: 'slide-down 0.2s ease-out' }}>
          <span>{notice}</span>
        </div>
      )}

      {tab === 'overview' && status && (
        <div style={{ display: 'grid', gap: '16px', gridTemplateColumns: 'repeat(auto-fit, minmax(280px, 1fr))', animation: 'fade-in 0.3s' }}>
          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', position: 'relative', overflow: 'hidden' }}>
            <div style={{ position: 'absolute', right: '-20px', top: '-20px', opacity: 0.05, transform: 'scale(2)' }}><Shield size={100} /></div>
            <div style={{ fontSize: '13px', color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '1px', fontWeight: '600' }}>Active Mode</div>
            <div style={{ fontSize: '32px', fontWeight: '700', textTransform: 'capitalize', color: 'var(--text-primary)', margin: '8px 0' }}>{status.mode}</div>
            <p style={{ fontSize: '13px', color: 'var(--text-secondary)', margin: 0, lineHeight: '1.5' }}>{MODES.find((m) => m.id === status.mode)?.text}</p>
          </div>
          
          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', position: 'relative', overflow: 'hidden' }}>
             <div style={{ position: 'absolute', right: '-20px', top: '-20px', opacity: 0.05, transform: 'scale(2)' }}><Wrench size={100} /></div>
            <div style={{ fontSize: '13px', color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '1px', fontWeight: '600' }}>Available Tools</div>
            <div style={{ fontSize: '32px', fontWeight: '700', color: 'var(--accent-cyan)', margin: '8px 0' }}>{status.tool_count}</div>
            <p style={{ fontSize: '13px', color: 'var(--text-secondary)', margin: 0 }}>{status.unavailable.length} more exist but cannot run on this machine.</p>
          </div>
          
          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column' }}>
            <div style={{ fontSize: '13px', color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '1px', fontWeight: '600', marginBottom: '8px' }}>Audit Trail Status</div>
            <button
              onClick={() =>
                void toolsApi
                  .verify()
                  .then((v) => setVerify(v.ok ? `Chain intact (${v.records ?? 0} records)${v.note ? ` — ${v.note}` : ''}` : `BROKEN: ${v.error ?? 'unknown'}`))
                  .catch((e) => setVerify(String(e)))
              }
              style={{
                padding: '8px 16px',
                borderRadius: '8px',
                border: '1px solid var(--glass-border)',
                background: 'rgba(255,255,255,0.05)',
                color: 'var(--text-primary)',
                cursor: 'pointer',
                fontSize: '13px',
                marginTop: 'auto',
                marginBottom: '12px',
                alignSelf: 'flex-start'
              }}
            >
              Verify Record Integrity
            </button>
            {verify && <p style={{ fontSize: '13px', color: verify.startsWith('BROKEN') ? 'var(--accent-rose)' : 'var(--accent-emerald)', margin: 0, fontWeight: '500' }}>{verify}</p>}
          </div>
          
          {status.unavailable.length > 0 && (
            <div className="glass-panel" style={{ gridColumn: '1 / -1', padding: '24px', borderRadius: '12px' }}>
              <div style={{ fontSize: '14px', fontWeight: '600', color: 'var(--text-primary)', marginBottom: '12px' }}>Unavailable Tooling</div>
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(300px, 1fr))', gap: '12px' }}>
                {[...new Map(status.unavailable.map((u) => [u.why, domainOf(u.tool)] as const)).entries()].map(([why, domain]) => (
                  <div key={why} style={{ fontSize: '13px', padding: '10px 14px', background: 'rgba(0,0,0,0.2)', borderRadius: '8px', border: '1px solid var(--glass-border)' }}>
                    <strong style={{ color: 'var(--text-primary)' }}>{domain}.*</strong> <span style={{ color: 'var(--text-secondary)', marginLeft: '8px' }}>{why}</span>
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>
      )}

      {tab === 'policy' && draft && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '20px', animation: 'fade-in 0.3s' }}>
          
          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px' }}>
            <div style={{ fontWeight: '700', fontSize: '16px', color: 'var(--text-primary)', marginBottom: '16px' }}>Enforcement Mode</div>
            <div style={{ display: 'grid', gap: '12px', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))' }}>
              {MODES.map((m) => (
                <label key={m.id} style={{ padding: '16px', borderRadius: '10px', background: draft.mode === m.id ? 'rgba(0, 240, 255, 0.1)' : 'rgba(0,0,0,0.2)', border: `1px solid ${draft.mode === m.id ? 'var(--accent-cyan)' : 'var(--glass-border)'}`, cursor: 'pointer', transition: 'all 0.2s', display: 'flex', flexDirection: 'column', gap: '8px' }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                    <input type="radio" name="mode" checked={draft.mode === m.id} onChange={() => setDraft({ ...draft, mode: m.id })} style={{ accentColor: 'var(--accent-cyan)' }} />
                    <strong style={{ color: draft.mode === m.id ? 'var(--accent-cyan)' : 'var(--text-primary)' }}>{m.title}</strong>
                  </div>
                  <div style={{ fontSize: '13px', color: 'var(--text-secondary)', lineHeight: '1.4' }}>{m.text}</div>
                </label>
              ))}
            </div>
            <p style={{ fontSize: '12px', color: 'var(--text-muted)', marginTop: '12px', marginBottom: 0 }}>Note: Setting `AQUA_TOOL_MODE` env var overrides this setting on restart.</p>
          </div>

          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px' }}>
            <div style={{ fontWeight: '700', fontSize: '16px', color: 'var(--text-primary)', marginBottom: '16px' }}>Rate Limits & Concurrency</div>
            <div style={{ display: 'grid', gap: '16px', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))' }}>
              {(
                [
                  ['reads_per_minute', 'Reads per minute/tool', 1, 10000],
                  ['changes_per_minute', 'Changes per minute/tool', 1, 10000],
                  ['max_concurrent', 'Concurrent Calls', 1, 64],
                ] as [keyof Limits, string, number, number][]
              ).map(([k, text, min, max]) => (
                <label key={k} style={{ display: 'flex', flexDirection: 'column', gap: '8px', fontSize: '13px', color: 'var(--text-secondary)', fontWeight: '500' }}>
                  {text}
                  <input
                    type="number"
                    min={min}
                    max={max}
                    value={draft.limits[k]}
                    onChange={(e) => setDraft({ ...draft, limits: { ...draft.limits, [k]: Math.max(min, Math.min(max, Number(e.target.value) || min)) } })}
                    style={{ padding: '10px 12px', borderRadius: '8px', border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-primary)', outline: 'none' }}
                  />
                </label>
              ))}
            </div>
          </div>

          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px' }}>
            <div style={{ fontWeight: '700', fontSize: '16px', color: 'var(--text-primary)', marginBottom: '4px' }}>Global Deny List</div>
            <div style={{ fontSize: '13px', color: 'var(--text-secondary)', marginBottom: '12px' }}>Tool names or whole groups (e.g. <code>term.*</code> or <code>ssh.upload</code>), one per line.</div>
            <textarea
              value={draft.hard_deny.join('\n')}
              onChange={(e) => setDraft({ ...draft, hard_deny: e.target.value.split('\n').map((x) => x.trim()).filter(Boolean) })}
              rows={4}
              style={{ padding: '12px', borderRadius: '8px', border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-primary)', fontFamily: 'ui-monospace, Consolas, monospace', fontSize: '13px', width: '100%', resize: 'vertical', outline: 'none' }}
            />
          </div>

          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px' }}>
            <div style={{ fontWeight: '700', fontSize: '16px', color: 'var(--text-primary)', marginBottom: '4px' }}>Tool-Specific Overrides</div>
            <div style={{ fontSize: '13px', color: 'var(--text-secondary)', marginBottom: '16px' }}>Overrides can only tighten a decision, never loosen it.</div>
            
            {Object.entries(draft.tool_overrides).map(([name, d]) => (
              <div key={name} style={{ display: 'flex', gap: '12px', alignItems: 'center', marginBottom: '12px', background: 'rgba(0,0,0,0.2)', padding: '8px 12px', borderRadius: '8px', border: '1px solid var(--glass-border)' }}>
                <code style={{ flex: 1, fontSize: '13px', color: 'var(--accent-purple)' }}>{name}</code>
                <select 
                  value={d} 
                  onChange={(e) => setDraft({ ...draft, tool_overrides: { ...draft.tool_overrides, [name]: e.target.value as Decision } })} 
                  style={{ padding: '6px 10px', borderRadius: '6px', border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.4)', color: 'var(--text-primary)', fontSize: '13px' }}
                >
                  {DECISIONS.map((x) => <option key={x} value={x}>{x}</option>)}
                </select>
                <button
                  onClick={() => {
                    const next = { ...draft.tool_overrides };
                    delete next[name];
                    setDraft({ ...draft, tool_overrides: next });
                  }}
                  style={{ padding: '6px 12px', borderRadius: '6px', border: '1px solid rgba(239, 68, 68, 0.3)', background: 'transparent', color: 'var(--accent-rose)', cursor: 'pointer', fontSize: '12px' }}
                >
                  Remove
                </button>
              </div>
            ))}
            
            <div style={{ display: 'flex', gap: '12px', marginTop: '16px', alignItems: 'center' }}>
              <select id="override-tool" defaultValue="" style={{ padding: '10px 12px', borderRadius: '8px', border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-primary)', fontSize: '13px', flex: 1 }}>
                <option value="" disabled>Select a tool to override…</option>
                {tools.map((t) => (
                  <option key={t.name} value={t.name}>{t.name}</option>
                ))}
              </select>
              <button
                onClick={() => {
                  const el = document.getElementById('override-tool') as HTMLSelectElement | null;
                  if (el?.value) setDraft({ ...draft, tool_overrides: { ...draft.tool_overrides, [el.value]: 'always_ask' } });
                }}
                style={{ padding: '10px 16px', borderRadius: '8px', border: '1px solid var(--glass-border)', background: 'rgba(255,255,255,0.05)', color: 'var(--text-primary)', cursor: 'pointer', fontSize: '13px', fontWeight: '600' }}
              >
                Add Override
              </button>
            </div>
          </div>

          <div style={{ display: 'flex', gap: '12px', alignItems: 'center', marginTop: '8px' }}>
            <button 
              disabled={!dirty || busy} 
              onClick={() => void save()} 
              style={{ 
                padding: '12px 24px', 
                borderRadius: '30px', 
                border: 'none', 
                background: dirty && !busy ? 'linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))' : 'var(--bg-tertiary)', 
                color: dirty && !busy ? '#fff' : 'var(--text-muted)', 
                fontWeight: '700', 
                cursor: dirty && !busy ? 'pointer' : 'not-allowed', 
                fontSize: '14px',
                boxShadow: dirty && !busy ? '0 4px 15px rgba(192, 132, 252, 0.3)' : 'none',
                transition: 'all 0.2s'
              }}
            >
              {busy ? 'Saving...' : 'Save Policy Changes'}
            </button>
            <button 
              disabled={!dirty || busy} 
              onClick={() => setDraft(policy)} 
              style={{ 
                padding: '12px 20px', 
                borderRadius: '30px', 
                border: '1px solid var(--glass-border)', 
                background: 'transparent', 
                color: 'var(--text-secondary)', 
                cursor: dirty && !busy ? 'pointer' : 'not-allowed',
                opacity: dirty && !busy ? 1 : 0.5
              }}
            >
              Discard Changes
            </button>
            {dirty && <span style={{ fontSize: '13px', color: 'var(--accent-purple)', fontWeight: '600', animation: 'pulse 2s infinite' }}>Unsaved changes</span>}
          </div>
        </div>
      )}

      {tab === 'activity' && (
        <div className="glass-panel" style={{ padding: '0', borderRadius: '12px', overflow: 'hidden', animation: 'fade-in 0.3s' }}>
          <div style={{ padding: '16px 20px', borderBottom: '1px solid var(--glass-border)', fontSize: '13px', color: 'var(--text-secondary)' }}>
            Real-time audit log. Newest first. Arguments are shown as recorded, with secrets masked.
          </div>
          <div style={{ overflowX: 'auto' }}>
            <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: '13px' }}>
              <thead>
                <tr style={{ textAlign: 'left', color: 'var(--text-secondary)', background: 'rgba(0,0,0,0.2)' }}>
                  <th style={{ padding: '12px 20px', fontWeight: '600' }}>Time</th>
                  <th style={{ padding: '12px 20px', fontWeight: '600' }}>Tool</th>
                  <th style={{ padding: '12px 20px', fontWeight: '600' }}>Caller</th>
                  <th style={{ padding: '12px 20px', fontWeight: '600' }}>Decision</th>
                  <th style={{ padding: '12px 20px', fontWeight: '600' }}>Result</th>
                  <th style={{ padding: '12px 20px', fontWeight: '600' }}>Arguments</th>
                </tr>
              </thead>
              <tbody>
                {records.map((r) => (
                  <tr key={r.seq} style={{ borderBottom: '1px solid var(--glass-border)' }}>
                    <td style={{ padding: '12px 20px', color: 'var(--text-primary)', whiteSpace: 'nowrap' }}>{new Date(r.ts_unix_ms).toLocaleTimeString()}</td>
                    <td style={{ padding: '12px 20px' }}><code style={{ color: 'var(--accent-cyan)' }}>{r.tool}</code></td>
                    <td style={{ padding: '12px 20px', color: 'var(--text-secondary)' }}>{r.caller}</td>
                    <td style={{ padding: '12px 20px' }}>
                      <span style={{ padding: '4px 8px', background: 'rgba(255,255,255,0.05)', borderRadius: '4px', fontSize: '12px', textTransform: 'uppercase' }}>
                        {r.decision.replace(/_/g, ' ')}
                      </span>
                    </td>
                    <td style={{ padding: '12px 20px', fontWeight: '600', color: r.ok ? 'var(--accent-emerald)' : r.error_code === 'STARTED' ? '#fbbf24' : 'var(--accent-rose)', textTransform: 'uppercase', fontSize: '12px' }}>
                      {r.ok ? 'ok' : r.error_code === 'STARTED' ? 'started' : r.error_code ?? 'failed'}
                    </td>
                    <td style={{ padding: '12px 20px', maxWidth: '300px', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontFamily: 'ui-monospace, Consolas, monospace', fontSize: '12px', color: 'var(--text-muted)' }} title={JSON.stringify(r.args)}>
                      {JSON.stringify(r.args)}
                    </td>
                  </tr>
                ))}
                {records.length === 0 && (
                  <tr><td colSpan={6} style={{ padding: '32px', textAlign: 'center', color: 'var(--text-muted)' }}>No tool calls recorded in this session.</td></tr>
                )}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {tab === 'tools' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '16px', animation: 'fade-in 0.3s' }}>
          {groups.map(([domain, list]) => (
            <div key={domain} className="glass-panel" style={{ padding: '20px', borderRadius: '12px' }}>
              <div style={{ fontWeight: '700', fontSize: '16px', color: 'var(--text-primary)', marginBottom: '12px', display: 'flex', alignItems: 'center', gap: '8px' }}>
                {domain}.* <span style={{ padding: '2px 8px', background: 'rgba(0, 240, 255, 0.1)', color: 'var(--accent-cyan)', borderRadius: '12px', fontSize: '12px' }}>{list.length}</span>
              </div>
              <div style={{ display: 'flex', flexDirection: 'column', gap: '8px' }}>
                {list.map((t) => (
                  <div key={t.name} style={{ display: 'flex', gap: '16px', padding: '12px', background: 'rgba(0,0,0,0.2)', borderRadius: '8px', border: '1px solid var(--glass-border)', flexWrap: 'wrap', alignItems: 'baseline' }}>
                    <code style={{ minWidth: '240px', color: 'var(--text-primary)', fontSize: '13px' }}>{t.signature}</code>
                    <span style={{ flex: 1, minWidth: '200px', fontSize: '13px', color: 'var(--text-secondary)' }}>{t.description}</span>
                    <span style={{ fontSize: '12px', color: 'var(--accent-purple)', padding: '2px 8px', background: 'rgba(192, 132, 252, 0.1)', borderRadius: '4px' }}>{t.classes.join(' · ')}</span>
                  </div>
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}


