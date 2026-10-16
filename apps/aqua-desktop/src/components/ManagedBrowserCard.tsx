import { useCallback, useEffect, useState, type CSSProperties } from 'react';
import { Globe, Play, Square, RotateCcw, Camera, Save } from 'lucide-react';
import { ApiImage } from './ApiImage';
import { browserApi, type BrowserPolicy, type BrowserStatus, type BackendKind } from '../api/browser';

const lines = (v: string[]) => v.join('\n');
const unlines = (v: string) => v.split('\n').map((s) => s.trim()).filter(Boolean);

/** Backend switch, status, site policy and session controls for Aqua's browser. */
export function ManagedBrowserCard() {
  const [status, setStatus] = useState<BrowserStatus | null>(null);
  const [policy, setPolicy] = useState<BrowserPolicy | null>(null);
  const [allowText, setAllowText] = useState('');
  const [denyText, setDenyText] = useState('');
  const [jsSitesText, setJsSitesText] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ tone: 'ok' | 'error'; text: string } | null>(null);
  const [confirmReset, setConfirmReset] = useState(false);
  const [screenshot, setScreenshot] = useState<string | null>(null);

  const loadPolicy = useCallback(async () => {
    const p = await browserApi.policy();
    setPolicy(p);
    setAllowText(lines(p.allow));
    setDenyText(lines(p.deny));
    setJsSitesText(lines(p.javascript_sites));
  }, []);

  useEffect(() => {
    void loadPolicy().catch(() => undefined);
    const poll = async () => {
      try {
        setStatus(await browserApi.status());
      } catch {
        setStatus(null);
      }
    };
    void poll();
    const id = window.setInterval(() => void poll(), 3000);
    return () => window.clearInterval(id);
  }, [loadPolicy]);

  const run = async (label: string, fn: () => Promise<unknown>) => {
    setBusy(true);
    setMessage(null);
    try {
      await fn();
      setMessage({ tone: 'ok', text: label });
      setStatus(await browserApi.status());
    } catch (e) {
      setMessage({ tone: 'error', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  };

  const savePolicy = () =>
    policy &&
    run('Policy saved', async () => {
      const saved = await browserApi.savePolicy({
        ...policy,
        allow: unlines(allowText),
        deny: unlines(denyText),
        javascript_sites: unlines(jsSitesText),
      });
      setPolicy(saved);
    });

  const takeScreenshot = () =>
    run('Screenshot saved', async () => {
      const obs = await browserApi.call('browser_take_screenshot', { type: 'png' });
      if (!obs.ok) throw new Error(obs.error?.message || 'Screenshot failed');
      const shot = obs.artifacts.find((a) => a.kind === 'screenshot');
      setScreenshot(shot ? `${browserApi.screenshotUrl(shot.path)}&t=${Date.now()}` : null);
    });

  const switchBackend = (b: BackendKind) => run(b === 'managed' ? 'Using Aqua browser' : 'Using your Chrome', () => browserApi.setBackend(b));

  const managed = status?.backend !== 'extension';

  return (
    <div className="glass-panel" style={{ padding: 24, borderRadius: 12, display: 'flex', flexDirection: 'column', gap: 16 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
          <Globe size={20} color="var(--accent-cyan)" />
          <div>
            <h3 style={{ fontSize: 18, fontWeight: 700, margin: 0 }}>Browser</h3>
            <p style={{ fontSize: 13, color: 'var(--text-secondary)', margin: 0 }}>
              {status
                ? `${managed ? `Aqua browser (${status.browser}, ${status.profile} profile${status.headless ? ', headless' : ''})` : 'Your Chrome via Browser MCP extension'} · ${
                    status.running ? `running, ${status.tabs} tab(s)` : 'starts on first use'
                  }`
                : 'Waiting for aqua-core…'}
            </p>
          </div>
        </div>
        <div role="group" aria-label="Browser backend" style={{ display: 'flex', gap: 6 }}>
          {(['managed', 'extension'] as BackendKind[]).map((b) => {
            const active = (b === 'managed') === managed;
            return (
              <button key={b} id={`browser-backend-${b}`} disabled={busy || active} onClick={() => void switchBackend(b)}
                style={{ ...pill, background: active ? 'rgba(6,182,212,0.18)' : 'rgba(255,255,255,0.04)', color: active ? 'var(--accent-cyan)' : 'var(--text-secondary)' }}>
                {b === 'managed' ? 'Aqua browser' : 'My Chrome'}
              </button>
            );
          })}
        </div>
      </div>

      {!managed && (
        <p style={{ margin: 0, padding: '10px 12px', borderRadius: 8, background: 'rgba(245,158,11,0.1)', fontSize: 13, color: '#fbbf24' }}>
          The agent is using your own Chrome profile, where you are signed in to your accounts. Switch back to the Aqua browser when you're done.
        </p>
      )}
      {status && managed && !status.installed && (
        <p style={{ margin: 0, fontSize: 13, color: '#fbbf24' }}>
          The Aqua browser isn't installed yet. Run <code>scripts/setup-managed-browser.ps1</code>, then refresh.
        </p>
      )}
      {status?.url && (
        <p style={{ margin: 0, fontSize: 12.5, color: 'var(--text-secondary)', wordBreak: 'break-all' }}>
          Current page: <code>{status.url}</code>
        </p>
      )}

      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <button id="browser-start" disabled={busy} onClick={() => void run('Browser started', browserApi.start)} style={action}><Play size={15} /> Start</button>
        <button id="browser-stop" disabled={busy} onClick={() => void run('Browser stopped', browserApi.stop)} style={action}><Square size={15} /> Stop</button>
        <button id="browser-screenshot" disabled={busy || !status?.running} onClick={() => void takeScreenshot()} style={action}><Camera size={15} /> Screenshot</button>
        {managed && !confirmReset && (
          <button id="browser-reset" disabled={busy} onClick={() => setConfirmReset(true)} style={action}><RotateCcw size={15} /> Reset profile</button>
        )}
        {managed && confirmReset && (
          <span style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13, color: 'var(--text-secondary)' }}>
            Sign out of every site in the Aqua browser?
            <button id="browser-reset-confirm" disabled={busy} onClick={() => { setConfirmReset(false); void run('Profile reset: all Aqua browser logins removed', browserApi.resetProfile); }} style={{ ...action, color: '#fca5a5' }}>Reset</button>
            <button id="browser-reset-cancel" onClick={() => setConfirmReset(false)} style={action}>Keep</button>
          </span>
        )}
      </div>
      {message && (
        <p role="status" style={{ margin: 0, fontSize: 13, color: message.tone === 'ok' ? '#34d399' : '#f87171' }}>{message.text}</p>
      )}
      {screenshot && (
        <ApiImage src={screenshot} alt="Latest screenshot of the Aqua browser" style={{ maxWidth: '100%', borderRadius: 8, border: '1px solid var(--glass-border)' }} />
      )}

      {policy && (
        <details>
          <summary style={{ cursor: 'pointer', fontWeight: 600, fontSize: 14 }}>Site and safety rules</summary>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 14, marginTop: 14 }}>
            <label style={field}>
              Unknown sites
              <select id="policy-mode" value={policy.mode} onChange={(e) => setPolicy({ ...policy, mode: e.target.value as BrowserPolicy['mode'] })} style={input}>
                <option value="ask">Ask me first</option>
                <option value="allowlist">Block (allowed list only)</option>
                <option value="denylist">Allow (except denied list)</option>
              </select>
            </label>
            <label style={field}>
              JavaScript from the agent
              <select id="policy-javascript" value={policy.javascript} onChange={(e) => setPolicy({ ...policy, javascript: e.target.value as BrowserPolicy['javascript'] })} style={input}>
                <option value="off">Off</option>
                <option value="allowed_sites">Only on sites listed below</option>
              </select>
            </label>
            <label style={field}>
              Browser
              <select id="policy-browser" value={policy.browser} onChange={(e) => setPolicy({ ...policy, browser: e.target.value as BrowserPolicy['browser'] })} style={input}>
                <option value="msedge">Microsoft Edge</option>
                <option value="chrome">Google Chrome</option>
              </select>
            </label>
            <label style={field}>
              Close when idle (minutes)
              <input id="policy-idle" type="number" min={1} max={240} value={policy.idle_shutdown_minutes}
                onChange={(e) => setPolicy({ ...policy, idle_shutdown_minutes: Number(e.target.value) || 10 })} style={input} />
            </label>
            <label style={{ ...field, flexDirection: 'row', alignItems: 'center', gap: 8 }}>
              <input id="policy-confirm" type="checkbox" checked={policy.confirm_irreversible} onChange={(e) => setPolicy({ ...policy, confirm_irreversible: e.target.checked })} />
              Ask before Send, Pay, Delete and password fields
            </label>
            <label style={{ ...field, flexDirection: 'row', alignItems: 'center', gap: 8 }}>
              <input id="policy-headless" type="checkbox" checked={policy.headless} onChange={(e) => setPolicy({ ...policy, headless: e.target.checked })} />
              Hide the browser window (headless)
            </label>
            <label style={field}>
              Allowed sites (one per line)
              <textarea id="policy-allow" rows={6} value={allowText} onChange={(e) => setAllowText(e.target.value)} style={{ ...input, fontFamily: 'monospace' }} />
            </label>
            <label style={field}>
              Denied sites (one per line; always wins)
              <textarea id="policy-deny" rows={6} value={denyText} onChange={(e) => setDenyText(e.target.value)} style={{ ...input, fontFamily: 'monospace' }} />
            </label>
            {policy.javascript === 'allowed_sites' && (
              <label style={field}>
                Sites where JavaScript is allowed
                <textarea id="policy-js-sites" rows={4} value={jsSitesText} onChange={(e) => setJsSitesText(e.target.value)} style={{ ...input, fontFamily: 'monospace' }} />
              </label>
            )}
          </div>
          <p style={{ fontSize: 12.5, color: 'var(--text-secondary)' }}>
            Rules accept <code>example.com</code>, <code>*.example.com</code>, <code>host:port</code> or <code>10.0.0.0/8</code>. Changing the browser, headless or idle time restarts the browser.
          </p>
          <button id="policy-save" disabled={busy} onClick={() => void savePolicy()} style={action}><Save size={15} /> Save rules</button>
        </details>
      )}
    </div>
  );
}

const pill: CSSProperties = { padding: '7px 14px', borderRadius: 999, border: '1px solid var(--glass-border)', fontWeight: 600, fontSize: 13, cursor: 'pointer' };
const action: CSSProperties = {
  padding: '8px 14px', borderRadius: 8, border: '1px solid var(--glass-border)', background: 'rgba(255,255,255,0.06)',
  color: 'var(--text-primary)', fontWeight: 600, cursor: 'pointer', display: 'flex', alignItems: 'center', gap: 6,
};
const field: CSSProperties = { display: 'flex', flexDirection: 'column', gap: 6, fontSize: 13, color: 'var(--text-secondary)' };
const input: CSSProperties = {
  padding: '8px 10px', borderRadius: 8, border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.25)', color: 'var(--text-primary)', fontSize: 13,
};
