import { Component, useCallback, useEffect, useRef, useState, type CSSProperties, type ReactNode } from 'react';
import { RefreshCw } from 'lucide-react';
import { AutonomyCard } from './AutonomyCard';
import { ConsentsPanel } from './ConsentsPanel';
import { FullAccessCard } from './FullAccessCard';
import { ManagedBrowserCard } from './ManagedBrowserCard';
import { PageHeader, Pill, Section } from './ui';
import { errorSentence } from '../api/plain';
import {
  callerText, canTrust, canUntrust, decisionText, envText, folderStanding, NOTES_FILE_HINT, permissionsPageApi, resultText, serverStanding, trustQuestion, untrustQuestion, whenText,
  type AuditRow, type McpList, type McpServer, type TrustedFolder,
} from '../api/permissionsPage';
import { useVisiblePolling } from '../hooks/useVisiblePolling';

const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const small: CSSProperties = { padding: '3px 10px' };
const line: CSSProperties = { display: 'flex', gap: 12, alignItems: 'flex-start', justifyContent: 'space-between', flexWrap: 'wrap', padding: '10px 0' };
const asText: CSSProperties = { fontFamily: 'var(--font-mono)', fontSize: 12.5, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' };
const refresh = (onClick: () => void) => (
  <button type="button" className="btn-ghost" style={{ ...small, display: 'inline-flex', alignItems: 'center', gap: 6 }} onClick={onClick}>
    <RefreshCw size={14} /> Refresh
  </button>
);
const input: CSSProperties = { padding: '8px 10px', borderRadius: 8, border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.25)', color: 'var(--text-primary)', fontSize: 13, flex: '1 1 320px', minWidth: 0 };

/** One section failing to draw (a value the app did not expect) must not blank the others: it says so, and the rest of the page stays. */
class SectionGuard extends Component<{ name: string; children: ReactNode }, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(error: unknown) {
    console.error('[ui] a section of the permissions page crashed:', this.props.name, error);
  }

  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <Section title={this.props.name}>
        <div role="alert" style={{ color: 'var(--state-problem)', fontSize: 13 }}>This part of the page could not be shown. The rest of the page still works.</div>
      </Section>
    );
  }
}

/** The state of one section that loads on its own: what it has, the last error sentence, the last result sentence. */
function useSection<T>(read: () => Promise<T>) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Only the newest answer is used: a slow read must not overwrite the one taken after a change.
  const latest = useRef(0);
  const load = useCallback(async () => {
    const mine = ++latest.current;
    try {
      const next = await read();
      if (mine !== latest.current) return;
      setData(next);
      setError(null);
    } catch (e) {
      if (mine !== latest.current) return;
      setError(errorSentence(e));
    }
  }, [read]);
  useEffect(() => {
    void load();
  }, [load]);
  return { data, error, setError, notice, setNotice, load };
}

function Messages({ error, notice }: { error: string | null; notice: string | null }) {
  return (
    <>
      {error && <div role="alert" style={{ color: 'var(--state-problem)', fontSize: 13 }}>{error}</div>}
      {notice && <div role="status" style={{ color: 'var(--state-ok)', fontSize: 13 }}>{notice}</div>}
    </>
  );
}

// ---- project folders whose notes file Aqua follows -----------------------------------------------------------------------------

const readFolders = () => permissionsPageApi.projects();

function TrustedFolders() {
  const { data, error, setError, notice, setNotice, load } = useSection<TrustedFolder[]>(readFolders);
  // The question being asked: to withdraw the trust of a folder, or to trust one (a new folder, or one whose file changed).
  const [asking, setAsking] = useState<{ action: 'trust' | 'untrust'; folder: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState('');

  const change = async (action: 'trust' | 'untrust', folder: string) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await permissionsPageApi.trustProject(folder, action === 'trust');
      setNotice(action === 'trust' ? 'Trusted the notes file in that folder, as it is now.' : 'Aqua no longer follows the notes file in that folder.');
      if (action === 'trust') setDraft('');
      await load();
    } catch (e) {
      setError(errorSentence(e));
    } finally {
      setBusy(false);
      setAsking(null);
    }
  };

  const typed = draft.trim();
  return (
    <Section
      title="Project folders you trust"
      hint={`A folder can hold ${NOTES_FILE_HINT} with notes for the agent about the project (how to build and test it). Aqua reads such a file only after you trust the folder, and only as it was when you trusted it: if it changes, Aqua ignores it until you trust it again. The notes cannot change what Aqua is allowed to do.`}
      actions={refresh(() => void load())}
    >
      <Messages error={error} notice={notice} />
      {data === null && !error && <div style={muted}>Loading…</div>}
      {data !== null && data.length === 0 && <div style={muted}>You have not trusted any project folder. Aqua tells the agent that a notes file exists, but does not read it.</div>}
      {data !== null && data.length > 0 && (
        <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
          {data.map((f, i) => {
            const standing = folderStanding(f);
            const mine = asking?.folder === f.folder ? asking : null;
            return (
              <li key={f.folder} style={{ ...line, borderTop: i === 0 ? 'none' : '1px solid var(--glass-border)' }}>
                <div style={{ flex: '1 1 320px', minWidth: 0, display: 'flex', flexDirection: 'column', gap: 4 }}>
                  <code style={asText}>{f.folder}</code>
                  <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                    <Pill tone={standing.tone} label={standing.text} />
                  </div>
                </div>
                <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                  {mine ? (
                    <>
                      <span style={{ fontSize: 13 }}>{mine.action === 'untrust' ? 'Stop following the notes file in this folder?' : 'Trust the notes file as it is now? Read it first.'}</span>
                      <button type="button" className="btn-danger" style={small} disabled={busy} onClick={() => void change(mine.action, f.folder)}>
                        {mine.action === 'untrust' ? 'Yes, stop trusting' : 'Yes, trust it'}
                      </button>
                      <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setAsking(null)}>Cancel</button>
                    </>
                  ) : (
                    <>
                      {standing.canTrustAgain && (
                        <button type="button" className="btn-ghost" style={small} disabled={busy} aria-label={`Trust the changed notes file again: ${f.folder}`} onClick={() => { setNotice(null); setAsking({ action: 'trust', folder: f.folder }); }}>Trust again</button>
                      )}
                      <button type="button" className="btn-ghost" style={small} disabled={busy} aria-label={`Stop trusting the folder: ${f.folder}`} onClick={() => { setNotice(null); setAsking({ action: 'untrust', folder: f.folder }); }}>Stop trusting</button>
                    </>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
      <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
          <input type="text" aria-label="A project folder to trust" placeholder="A folder inside the folders Aqua works in" value={draft} style={input}
            onChange={(e) => { setDraft(e.target.value); setAsking(null); }} />
          <button type="button" className="btn-ghost" style={small} disabled={busy || typed === ''} onClick={() => { setNotice(null); setAsking({ action: 'trust', folder: typed }); }}>Trust this folder…</button>
        </div>
        {asking && asking.action === 'trust' && asking.folder === typed && !(data ?? []).some((f) => f.folder === asking.folder) && (
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
            <span style={{ fontSize: 13 }}>Trust {NOTES_FILE_HINT} in this folder, as the file is now? The agent will be given its notes. Read it first.</span>
            <button type="button" className="btn-danger" style={small} disabled={busy} onClick={() => void change('trust', typed)}>Yes, trust it</button>
            <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setAsking(null)}>Cancel</button>
          </div>
        )}
      </div>
    </Section>
  );
}

// ---- programs added in mcp.json ---------------------------------------------------------------------------------------------------

const readServers = () => permissionsPageApi.servers();

function McpServers() {
  const { data, error, setError, notice, setNotice, load } = useSection<McpList>(readServers);
  const [asking, setAsking] = useState<{ action: 'trust' | 'untrust'; id: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const change = async (action: 'trust' | 'untrust', server: McpServer) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      if (action === 'trust') await permissionsPageApi.trustServer(server.id);
      else await permissionsPageApi.untrustServer(server.id);
      setNotice(action === 'trust' ? `Trusted "${server.id}". It is started and its tools are added.` : `Stopped trusting "${server.id}". It is stopped and its tools are gone.`);
      await load();
    } catch (e) {
      setError(errorSentence(e));
    } finally {
      setBusy(false);
      setAsking(null);
    }
  };

  return (
    <Section
      title="Programs you added (MCP servers)"
      hint="Programs listed in your mcp.json give Aqua more tools. Nothing of a program is started until you trust it, exactly as its command line is written here. If you change the entry, the trust ends and you are asked again."
      actions={refresh(() => void load())}
    >
      <Messages error={error} notice={notice} />
      {data === null && !error && <div style={muted}>Loading…</div>}
      {data !== null && data.problem && <div role="alert" style={{ color: 'var(--state-attention)', fontSize: 13, overflowWrap: 'anywhere' }}>{data.problem}</div>}
      {data !== null && data.servers.length === 0 && !data.problem && <div style={muted}>You have not added any program. Add one to mcp.json and it is listed here, not trusted.</div>}
      {data !== null && data.servers.length > 0 && (
        <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
          {data.servers.map((s, i) => {
            const standing = serverStanding(s);
            const mine = asking?.id === s.id ? asking : null;
            return (
              <li key={s.id} style={{ ...line, flexDirection: 'column', alignItems: 'stretch', borderTop: i === 0 ? 'none' : '1px solid var(--glass-border)' }}>
                <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap' }}>
                  <strong style={{ fontSize: 14, overflowWrap: 'anywhere' }}>{s.id}</strong>
                  <Pill tone={standing.tone} label={standing.text} />
                </div>
                <div style={muted}>It is started with this command line:</div>
                <code style={{ ...asText, padding: '8px 10px', borderRadius: 8, background: 'rgba(0,0,0,0.25)' }}>{s.commandLine === '' ? '(the core sent no command line)' : s.commandLine}</code>
                {s.cwd && <div style={muted}>In the folder: <code style={asText}>{s.cwd}</code></div>}
                {s.env.length > 0 && (
                  <details>
                    <summary style={{ cursor: 'pointer', fontSize: 13, color: 'var(--text-secondary)' }}>Variables it is started with ({s.env.length})</summary>
                    <ul style={{ margin: '6px 0 0', paddingLeft: 18 }}>
                      {s.env.map((e, k) => <li key={`${e.name}-${k}`} style={asText}>{envText(e)}</li>)}
                    </ul>
                  </details>
                )}
                {s.toolsAllow.length > 0 && <div style={muted}>Only these tools may be used: {s.toolsAllow.join(', ')}</div>}
                {s.tools.length > 0 && (
                  <details>
                    <summary style={{ cursor: 'pointer', fontSize: 13, color: 'var(--text-secondary)' }}>{s.tools.length === 1 ? '1 tool' : `${s.tools.length} tools`}</summary>
                    <ul style={{ margin: '6px 0 0', paddingLeft: 18, display: 'flex', flexDirection: 'column', gap: 4 }}>
                      {s.tools.map((t, k) => (
                        <li key={`${t.name}-${k}`} style={{ fontSize: 12.5, overflowWrap: 'anywhere' }}>
                          <code>{t.name}</code>{t.readOnly ? ' (reads only)' : ''}{t.description ? `: ${t.description}` : ''}
                        </li>
                      ))}
                    </ul>
                  </details>
                )}
                <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                  {mine ? (
                    <>
                      <span style={{ fontSize: 13 }}>{mine.action === 'trust' ? trustQuestion(s) : untrustQuestion(s)}</span>
                      <button type="button" className="btn-danger" style={small} disabled={busy} onClick={() => void change(mine.action, s)}>
                        {mine.action === 'trust' ? 'Yes, trust it and start it' : 'Yes, stop trusting it'}
                      </button>
                      <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setAsking(null)}>Cancel</button>
                    </>
                  ) : (
                    <>
                      {canTrust(s) && (
                        <button type="button" className="btn-ghost" style={small} disabled={busy} aria-label={`Trust the program: ${s.id}`} onClick={() => { setNotice(null); setAsking({ action: 'trust', id: s.id }); }}>
                          {s.trusted ? 'Trust again' : 'Trust…'}
                        </button>
                      )}
                      {canUntrust(s) && (
                        <button type="button" className="btn-ghost" style={small} disabled={busy} aria-label={`Stop trusting the program: ${s.id}`} onClick={() => { setNotice(null); setAsking({ action: 'untrust', id: s.id }); }}>Stop trusting</button>
                      )}
                    </>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </Section>
  );
}

// ---- the last tool calls ------------------------------------------------------------------------------------------------------------

const readCalls = () => permissionsPageApi.recentCalls(20);
const th: CSSProperties = { textAlign: 'left', padding: '8px 10px', fontWeight: 600, color: 'var(--text-secondary)', whiteSpace: 'nowrap' };
const td: CSSProperties = { padding: '8px 10px', verticalAlign: 'top', overflowWrap: 'anywhere' };

function RecentCalls() {
  const { data, error, load } = useSection<AuditRow[]>(readCalls);
  useVisiblePolling(() => void load(), 5000);
  const now = Date.now() / 1000;
  return (
    <Section
      title="The last tool calls"
      hint="What the tools did lately, newest first: what ran, who asked for it, and what was decided. The list is read every few seconds while this page is open."
      actions={refresh(() => void load())}
    >
      {error && <div role="alert" style={{ color: 'var(--state-problem)', fontSize: 13 }}>{error}</div>}
      {data === null && !error && <div style={muted}>Loading…</div>}
      {data !== null && data.length === 0 && <div style={muted}>No tool call has been recorded yet.</div>}
      {data !== null && data.length > 0 && (
        <div style={{ overflowX: 'auto' }}>
          <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 13 }}>
            <thead>
              <tr>
                <th style={th}>When</th>
                <th style={th}>What ran</th>
                <th style={th}>Who asked</th>
                <th style={th}>What was decided</th>
                <th style={th}>Result</th>
              </tr>
            </thead>
            <tbody>
              {data.map((row, i) => (
                <tr key={`${row.seq ?? 'x'}-${i}`} style={{ borderTop: '1px solid var(--glass-border)' }}>
                  <td style={{ ...td, whiteSpace: 'nowrap' }}>{whenText(row, now)}</td>
                  <td style={td}><code>{row.tool === '' ? 'not recorded' : row.tool}</code></td>
                  <td style={td}>{callerText(row.caller)}</td>
                  <td style={td}>{decisionText(row.decision)}</td>
                  <td style={td}>{resultText(row)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Section>
  );
}

// ---- the page -------------------------------------------------------------------------------------------------------------------------

/**
 * What Aqua may do, in one place: full access for the project, how much Aqua asks and how much one run may do, the browser's rules, what you have said yes to, the folders and programs you
 * trust, and the last tool calls. Each part loads on its own, so one that cannot be read, or cannot be drawn, does not hide the others.
 * Everything that comes from the core is a text child, never markup.
 */
export function PermissionsPage() {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 24, maxWidth: 1200, margin: '0 auto', width: '100%' }}>
      <PageHeader
        title="Permissions and consents"
        subtitle="What Aqua may do without asking, what you have said yes to, which folders and programs you trust, and what the tools did lately. The tool mode and the deny list are on the page Tools and approvals."
      />
      <SectionGuard name="Full access for this project"><FullAccessCard /></SectionGuard>
      <AutonomyCard />
      <SectionGuard name="Browser"><ManagedBrowserCard /></SectionGuard>
      <SectionGuard name="What you have said yes to"><ConsentsPanel /></SectionGuard>
      <SectionGuard name="Project folders you trust"><TrustedFolders /></SectionGuard>
      <SectionGuard name="Programs you added (MCP servers)"><McpServers /></SectionGuard>
      <SectionGuard name="The last tool calls"><RecentCalls /></SectionGuard>
    </div>
  );
}
