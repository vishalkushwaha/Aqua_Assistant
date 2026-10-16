import { useCallback, useEffect, useState, type CSSProperties } from 'react';
import { learningApi, type LaunchSkill, type LaunchSkills } from '../api/learning';

const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const cell: CSSProperties = { padding: '7px 8px', fontSize: 13 };

const HOW: Record<LaunchSkill['recipe']['kind'], string> = {
  exe: 'program name',
  path: 'program file',
  apps_folder: 'Start menu entry',
  uri: 'settings page',
};

function ago(unix: number): string {
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 90) return 'just now';
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 86_400 * 2) return `${Math.round(s / 3600)} h ago`;
  return new Date(unix * 1000).toLocaleDateString();
}

/** What Aqua has learned about opening apps: the way that worked, how sure it is, how long it takes. */
export function LaunchSkillsCard() {
  const [data, setData] = useState<LaunchSkills | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmAll, setConfirmAll] = useState(false);

  const load = useCallback(() => {
    learningApi.launchSkills().then(
      (d) => {
        setData(d);
        setError(null);
      },
      (e: unknown) => setError(e instanceof Error ? e.message : String(e)),
    );
  }, []);

  useEffect(() => {
    load();
    const id = window.setInterval(() => {
      if (document.visibilityState === 'visible' && document.hasFocus()) load();
    }, 4000);
    return () => window.clearInterval(id);
  }, [load]);

  const forget = (key?: string) => {
    const p = key ? learningApi.forgetLaunchSkill(key) : learningApi.forgetAllLaunchSkills();
    void p.then(load, (e: unknown) => setError(e instanceof Error ? e.message : String(e)));
    setConfirmAll(false);
  };

  return (
    <div className="glass-panel" style={{ padding: 24, borderRadius: 12, display: 'flex', flexDirection: 'column', gap: 12 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: 12, flexWrap: 'wrap' }}>
        <div>
          <h3 style={{ margin: 0, fontSize: 16, color: 'var(--text-primary)', fontWeight: 600 }}>Learned app shortcuts</h3>
          <p style={{ ...muted, marginTop: 4, maxWidth: '70ch', lineHeight: 1.5 }}>
            When Aqua opens an app and sees its window appear, it remembers how and is surer next time; a way that fails is penalised, and dropped after two failures in a row. Opening an app you have used before takes a fraction of a second instead of seconds.
          </p>
        </div>
        {data && data.skills.length > 0 &&
          (!confirmAll ? (
            <button className="btn-ghost" onClick={() => setConfirmAll(true)}>Forget all</button>
          ) : (
            <span style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13 }}>
              Forget every learned way?
              <button className="btn-danger solid" onClick={() => forget()}>Yes, forget</button>
              <button className="btn-ghost" onClick={() => setConfirmAll(false)}>Keep</button>
            </span>
          ))}
      </div>
      {error && <div role="alert" style={{ color: 'var(--state-problem)', fontSize: 13 }}>{error}</div>}
      {data && !data.learning_enabled && (
        <div style={{ color: 'var(--state-attention)', fontSize: 13 }}>
          Agent Learning is switched off: nothing new is learned and learned ways are not used. Programs that ship with Windows still open quickly.
        </div>
      )}
      {data && data.learning_enabled && !data.used_for_new_requests && (
        <div style={{ color: 'var(--state-attention)', fontSize: 13 }}>Memory Retrieval is switched off: Aqua keeps learning but does not use what it learned.</div>
      )}
      {data && data.skills.length === 0 ? (
        <div style={muted}>Nothing yet. Say "open Notepad" (or any app) and it will appear here.</div>
      ) : (
        data && (
          <div style={{ overflowX: 'auto' }}>
            <table style={{ width: '100%', borderCollapse: 'collapse' }}>
              <thead>
                <tr style={{ textAlign: 'left', color: 'var(--text-secondary)', fontSize: 12.5 }}>
                  <th style={cell}>App</th>
                  <th style={cell}>How it is opened</th>
                  <th style={cell}>Sure</th>
                  <th style={cell}>Worked</th>
                  <th style={cell}>Failed</th>
                  <th style={cell}>Usual time</th>
                  <th style={cell}>Last used</th>
                  <th style={cell} />
                </tr>
              </thead>
              <tbody>
                {data.skills.map((s) => (
                  <tr key={s.key} style={{ borderTop: '1px solid var(--glass-border)' }}>
                    <td style={cell}><strong>{s.name}</strong></td>
                    <td style={cell} title={s.recipe.target}>
                      {HOW[s.recipe.kind]} <span style={muted}>({s.source})</span>
                    </td>
                    <td style={cell}>
                      <span style={{ display: 'inline-block', width: 60, height: 6, borderRadius: 3, background: 'rgba(255,255,255,0.1)', verticalAlign: 'middle', marginRight: 6 }}>
                        <span style={{ display: 'block', width: `${Math.round(s.confidence * 100)}%`, height: '100%', borderRadius: 3, background: s.confidence >= 0.5 ? 'var(--state-ok)' : 'var(--state-attention)' }} />
                      </span>
                      {Math.round(s.confidence * 100)}%
                    </td>
                    <td style={cell}>{s.successes}</td>
                    <td style={cell}>{s.failures}</td>
                    <td style={cell}>{s.avg_ms ? `${s.avg_ms} ms` : '-'}</td>
                    <td style={cell}>{ago(s.last_unix)}</td>
                    <td style={cell}>
                      <button className="btn-ghost" style={{ padding: '3px 10px' }} onClick={() => forget(s.key)}>Forget</button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <div style={{ ...muted, marginTop: 8 }}>
              {data.totals.successes} launches seen to work, {data.totals.failures} failed.
            </div>
          </div>
        )
      )}
    </div>
  );
}
