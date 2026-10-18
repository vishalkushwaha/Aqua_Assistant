import { useEffect, useState } from 'react';
import { CATEGORY_LABELS, devicesApi, useDevices, type Category, type DevicePrefs, type DeviceSettings, type DeviceView } from '../api/devices';
import { PageHeader, Pill, Section, Switch, UnavailableState, type Tone } from './ui';

const INPUT_STYLE = {
  width: 72,
  padding: '8px 10px',
  borderRadius: 8,
  border: '1px solid var(--glass-border)',
  background: 'rgba(0,0,0,0.3)',
  color: 'var(--text-primary)',
  outline: 'none',
} as const;

const SELECT_STYLE = { ...INPUT_STYLE, width: 'auto', padding: '5px 8px', fontSize: 13 } as const;

type Tri = 'default' | 'on' | 'off';
const toTri = (v: boolean | null): Tri => (v === null ? 'default' : v ? 'on' : 'off');
const fromTri = (v: Tri): boolean | null => (v === 'default' ? null : v === 'on');

function batteryText(d: DeviceView): { text: string; tone: Tone } {
  if (!d.connected) return { text: '—', tone: 'neutral' };
  if (d.battery_percent === null) return { text: 'not reported by this device', tone: 'neutral' };
  const tone: Tone = d.battery_state === 'critical' || d.battery_state === 'very_low' ? 'problem' : d.battery_state === 'low' ? 'attention' : 'ok';
  const charge = d.charging === 'charging' ? ', charging' : d.charging === 'full' ? ', full' : '';
  return { text: `${d.battery_percent}%${charge}`, tone };
}

function relative(seconds: number): string {
  if (seconds < 5) return 'just now';
  if (seconds < 90) return `${Math.round(seconds)} s ago`;
  if (seconds < 5400) return `${Math.round(seconds / 60)} min ago`;
  return `${Math.round(seconds / 3600)} h ago`;
}

function lastSeen(unix: number): string {
  if (!unix) return 'unknown';
  const secs = Math.max(0, Date.now() / 1000 - unix);
  return secs < 90 ? 'just now' : secs < 86_400 * 2 ? relative(secs) : new Date(unix * 1000).toLocaleDateString();
}

/** The "Devices and battery" page: what Aqua announces when a device connects or its battery runs low, and what it said. */
export function DevicesPanel() {
  const snap = useDevices();
  const [error, setError] = useState<string | null>(null);
  const [confirmForgetAll, setConfirmForgetAll] = useState(false);
  const [levels, setLevels] = useState<{ low: string; very: string; crit: string } | null>(null);
  const [showDecisions, setShowDecisions] = useState(true);

  // keep the three level inputs in step with the saved settings until the user edits them
  useEffect(() => {
    if (snap && levels === null) setLevels({ low: String(snap.settings.low_percent), very: String(snap.settings.very_low_percent), crit: String(snap.settings.critical_percent) });
  }, [snap, levels]);

  const act = async (p: Promise<{ ok: boolean; error?: string }>) => {
    const r = await p;
    setError(r.ok ? null : (r.error ?? 'That did not work.'));
  };

  if (!snap) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 1000, margin: '0 auto' }}>
        <PageHeader title="Audio Devices & Battery" subtitle="Local connection, battery, and announcement controls for your devices." />
        <UnavailableState
          title="Waiting for device monitoring"
          description="Start Aqua Core to load microphones, speakers, Bluetooth devices, battery levels, notification history, and quiet-hour controls."
          status="Device monitor offline"
        />
      </div>
    );
  }
  const s = snap.settings;
  const set = (patch: Partial<DeviceSettings>) => void act(devicesApi.settings(patch));
  const sourceProblems = Object.entries(snap.sources).filter(([, h]) => h.state !== 'ok');

  const levelNumbers = levels ? { low: Number(levels.low), very: Number(levels.very), crit: Number(levels.crit) } : null;
  const levelProblem =
    levelNumbers && (![levelNumbers.low, levelNumbers.very, levelNumbers.crit].every((n) => Number.isInteger(n) && n >= 1 && n <= 99) || !(levelNumbers.low > levelNumbers.very && levelNumbers.very > levelNumbers.crit))
      ? 'The levels must go low > very low > critical, each between 1 and 99.'
      : null;
  const levelsChanged = levelNumbers && (levelNumbers.low !== s.low_percent || levelNumbers.very !== s.very_low_percent || levelNumbers.crit !== s.critical_percent);

  const setPrefs = (d: DeviceView, patch: Partial<DevicePrefs>) => void act(devicesApi.deviceSettings(d.ref, { ...d.prefs, ...patch }));
  const toggleCategory = (c: Category, on: boolean) => {
    const rest = s.disabled_categories.filter((x) => x !== c);
    set({ disabled_categories: on ? rest : [...rest, c] });
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 1000, margin: '0 auto' }}>
      <PageHeader
        title="Audio Devices & Battery"
        subtitle="Aqua tells you by voice when a device connects, and when the battery of a device (or of this computer) runs low. It uses Windows' own notifications, so it costs nothing while nothing changes. Nothing leaves this computer."
        actions={
          <>
            <button className="btn-ghost" disabled={!s.enabled} onClick={() => void act(devicesApi.test())}>
              Test announcement
            </button>
            <button className="btn-primary" onClick={() => set({ enabled: !s.enabled })}>
              {s.enabled ? 'Turn off' : 'Turn on'}
            </button>
          </>
        }
      />

      {error && (
        <div role="alert" className="glass-panel" style={{ padding: 14, color: 'var(--state-problem)' }}>
          {error}
        </div>
      )}
      {snap.warning && (
        <div className="glass-panel" style={{ padding: 14, color: 'var(--state-attention)' }}>
          {snap.warning}
        </div>
      )}
      {sourceProblems.map(([name, h]) => (
        <div key={name} className="glass-panel" style={{ padding: 14, color: h.state === 'failed' ? 'var(--state-problem)' : 'var(--state-attention)' }}>
          Device detection: {h.reason ?? h.state}
        </div>
      ))}

      <Section title="What Aqua says" hint={s.enabled ? 'Each switch is explained in one line. The switches next to each device below can override these for that device.' : 'Device monitoring is off: Aqua says nothing about devices. Devices are still tracked, so turning it on again is instant.'}>
        <Switch disabled={!s.enabled} label="A device connects" hint="“Rockerz 551ANC connected.” Several at once become one sentence." checked={s.announce_connections} onChange={(v) => set({ announce_connections: v })} />
        <Switch disabled={!s.enabled} label="A device disconnects" hint="Off by default: it is the noisiest. A device that disconnects while its battery is nearly empty is always reported." checked={s.announce_disconnections} onChange={(v) => set({ announce_disconnections: v })} />
        <Switch disabled={!s.enabled} label="Low and very low battery" hint="Once when the level is crossed, never repeated while it stays low." checked={s.announce_low_battery} onChange={(v) => set({ announce_low_battery: v })} />
        <Switch disabled={!s.enabled} label="Critical battery" hint="“Warning. Your headphones are critically low on battery.” Spoken before anything else." checked={s.announce_critical_battery} onChange={(v) => set({ announce_critical_battery: v })} />
        <Switch disabled={!s.enabled} label="Charging starts" hint="Only for devices that tell Windows they are charging. Most Bluetooth headphones do not." checked={s.announce_charging} onChange={(v) => set({ announce_charging: v })} />
        <Switch disabled={!s.enabled} label="Fully charged" checked={s.announce_fully_charged} onChange={(v) => set({ announce_fully_charged: v })} />
        <Switch disabled={!s.enabled} label="Battery recovered after a warning" checked={s.announce_recovered} onChange={(v) => set({ announce_recovered: v })} />
        <Switch disabled={!s.enabled} label="Unknown USB devices" hint="“Unknown USB device connected.” Also a useful warning if a device appears that you did not plug in." checked={s.announce_unknown_usb} onChange={(v) => set({ announce_unknown_usb: v })} />
      </Section>

      <Section title="Battery levels" hint="Aqua warns once when the battery falls to each level. Low must be above very low, which must be above critical.">
        {levels && (
          <div style={{ display: 'flex', gap: 18, flexWrap: 'wrap', alignItems: 'center' }}>
            {([['Low', 'low'], ['Very low', 'very'], ['Critical', 'crit']] as const).map(([label, key]) => (
              <label key={key} style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 14 }}>
                {label}
                <input type="number" min={1} max={99} value={levels[key]} onChange={(e) => setLevels({ ...levels, [key]: e.target.value })} style={INPUT_STYLE} aria-label={`${label} battery level in percent`} />%
              </label>
            ))}
            <button
              className="btn-ghost"
              disabled={!!levelProblem || !levelsChanged}
              onClick={() => levelNumbers && set({ low_percent: levelNumbers.low, very_low_percent: levelNumbers.very, critical_percent: levelNumbers.crit })}
            >
              Save levels
            </button>
          </div>
        )}
        {levelProblem && <p style={{ color: 'var(--state-attention)', fontSize: 13.5 }}>{levelProblem}</p>}
        <div style={{ display: 'flex', gap: 18, flexWrap: 'wrap', alignItems: 'center', fontSize: 14 }}>
          <label style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
            Repeat a critical warning
            <select style={SELECT_STYLE} value={s.repeat_critical_minutes} onChange={(e) => set({ repeat_critical_minutes: Number(e.target.value) })}>
              <option value={0}>never</option>
              <option value={5}>every 5 minutes</option>
              <option value={10}>every 10 minutes</option>
              <option value={30}>every 30 minutes</option>
            </select>
          </label>
          <Switch label="Treat a rising level as charging (a guess)" hint="For devices that do not say. Off unless you want it." checked={s.infer_charging} onChange={(v) => set({ infer_charging: v })} />
        </div>
      </Section>

      <Section title="Quiet times" hint="Only critical battery warnings are spoken while a full-screen app or a presentation is running, and during quiet hours.">
        <Switch label="Quiet hours" checked={s.quiet_hours.enabled} onChange={(v) => set({ quiet_hours: { ...s.quiet_hours, enabled: v } })} />
        {s.quiet_hours.enabled && (
          <div style={{ display: 'flex', gap: 14, alignItems: 'center', fontSize: 14 }}>
            From
            <select style={SELECT_STYLE} value={s.quiet_hours.start_hour} onChange={(e) => set({ quiet_hours: { ...s.quiet_hours, start_hour: Number(e.target.value) } })}>
              {Array.from({ length: 24 }, (_, h) => <option key={h} value={h}>{String(h).padStart(2, '0')}:00</option>)}
            </select>
            to
            <select style={SELECT_STYLE} value={s.quiet_hours.end_hour} onChange={(e) => set({ quiet_hours: { ...s.quiet_hours, end_hour: Number(e.target.value) } })}>
              {Array.from({ length: 24 }, (_, h) => <option key={h} value={h}>{String(h).padStart(2, '0')}:00</option>)}
            </select>
          </div>
        )}
      </Section>

      <Section title="Kinds of device" hint="Switch off a whole kind. A single device can be switched off in the list below.">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(230px, 1fr))', gap: 10 }}>
          {(Object.keys(CATEGORY_LABELS) as Category[]).map((c) => (
            <Switch key={c} label={CATEGORY_LABELS[c]} checked={!s.disabled_categories.includes(c)} onChange={(v) => toggleCategory(c, v)} />
          ))}
        </div>
      </Section>

      <Section
        title="Devices"
        hint="Every device Aqua can see now, and ones it has seen before. “Default” follows the switches above; On or Off here is for this device only. Ignore stops every announcement about it."
        actions={
          !confirmForgetAll ? (
            <button className="btn-ghost" onClick={() => setConfirmForgetAll(true)}>
              Forget all devices
            </button>
          ) : (
            <span style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13.5 }}>
              Remove every remembered device and its settings?
              <button className="btn-danger solid" onClick={() => { setConfirmForgetAll(false); void act(devicesApi.forgetAll()); }}>Yes, forget</button>
              <button className="btn-ghost" onClick={() => setConfirmForgetAll(false)}>Keep</button>
            </span>
          )
        }
      >
        {snap.devices.length === 0 ? (
          <p style={{ fontSize: 14, color: 'var(--text-secondary)' }}>No external devices yet. Connect headphones, a keyboard or a USB drive and it will appear here.</p>
        ) : (
          <div style={{ overflowX: 'auto' }}>
            <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 13.5 }}>
              <thead>
                <tr style={{ textAlign: 'left', color: 'var(--text-secondary)' }}>
                  <th style={{ padding: '6px 8px' }}>Device</th>
                  <th style={{ padding: '6px 8px' }}>Now</th>
                  <th style={{ padding: '6px 8px' }}>Battery</th>
                  <th style={{ padding: '6px 8px' }}>Connect</th>
                  <th style={{ padding: '6px 8px' }}>Disconnect</th>
                  <th style={{ padding: '6px 8px' }}>Battery warnings</th>
                  <th style={{ padding: '6px 8px' }}>Charging</th>
                  <th style={{ padding: '6px 8px' }}>Ignore</th>
                  <th style={{ padding: '6px 8px' }} />
                </tr>
              </thead>
              <tbody>
                {snap.devices.map((d) => {
                  const b = batteryText(d);
                  const tri = (key: 'connect' | 'disconnect' | 'battery' | 'charging') => (
                    <select style={SELECT_STYLE} value={toTri(d.prefs[key])} disabled={d.prefs.ignore} aria-label={`${d.name}: ${key}`} onChange={(e) => setPrefs(d, { [key]: fromTri(e.target.value as Tri) })}>
                      <option value="default">Default</option>
                      <option value="on">On</option>
                      <option value="off">Off</option>
                    </select>
                  );
                  return (
                    <tr key={d.ref} style={{ borderTop: '1px solid var(--glass-border)', opacity: d.prefs.ignore ? 0.6 : 1 }}>
                      <td style={{ padding: '8px' }}>
                        <strong>{d.name}</strong>
                        {d.same_name_as_another && <span style={{ color: 'var(--text-muted)' }}> · {d.ref.slice(0, 4)}</span>}
                        <div style={{ color: 'var(--text-secondary)', fontSize: 12.5 }}>
                          {d.type_text} · {d.connection}
                          {d.id_kind === 'weak' || d.id_kind === 'container' ? ' · identified by port or name' : ''}
                        </div>
                      </td>
                      <td style={{ padding: '8px' }}>
                        <Pill tone={d.connected ? 'ok' : 'neutral'} label={d.connected ? 'Connected' : `Seen ${lastSeen(d.last_seen)}`} />
                      </td>
                      <td style={{ padding: '8px' }}>
                        <Pill tone={b.tone} label={b.text} />
                      </td>
                      <td style={{ padding: '8px' }}>{tri('connect')}</td>
                      <td style={{ padding: '8px' }}>{tri('disconnect')}</td>
                      <td style={{ padding: '8px' }}>{tri('battery')}</td>
                      <td style={{ padding: '8px' }}>{tri('charging')}</td>
                      <td style={{ padding: '8px' }}>
                        <input type="checkbox" checked={d.prefs.ignore} aria-label={`Ignore ${d.name}`} onChange={(e) => setPrefs(d, { ignore: e.target.checked })} style={{ accentColor: 'var(--accent-cyan)' }} />
                      </td>
                      <td style={{ padding: '8px' }}>
                        <button className="btn-ghost" onClick={() => void act(devicesApi.forget(d.ref))} style={{ padding: '4px 10px' }}>
                          Forget
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Section>

      <Section
        title="What happened"
        hint="Why Aqua spoke, and why it stayed quiet."
        actions={
          <button className="btn-ghost" onClick={() => setShowDecisions((v) => !v)}>
            {showDecisions ? 'Hide' : 'Show'}
          </button>
        }
      >
        <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
          <Pill label="Events seen" value={snap.counters.events} />
          <Pill tone="ok" label="Announced" value={snap.counters.announced} />
          <Pill label="Kept quiet" value={snap.counters.suppressed} />
          {snap.pending > 0 && <Pill tone="attention" label="Waiting to be spoken" value={snap.pending} />}
          {snap.cpu_seconds && <Pill label="Effort so far" value={`${(snap.cpu_seconds.engine + snap.cpu_seconds.source).toFixed(2)} s of processor time`} title="Processor time used by the device monitor since Aqua started." />}
        </div>
        {showDecisions &&
          (snap.recent.length === 0 ? (
            <p style={{ fontSize: 14, color: 'var(--text-secondary)' }}>Nothing yet.</p>
          ) : (
            <ul style={{ listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 6, fontSize: 13.5 }}>
              {snap.recent.slice(0, 25).map((d, i) => (
                <li key={i} style={{ display: 'flex', gap: 10, alignItems: 'baseline' }}>
                  <span style={{ color: 'var(--text-muted)', minWidth: 70 }}>{relative(Math.max(0, (snap.now_ms - d.at_ms) / 1000))}</span>
                  <span style={{ color: d.result === 'announced' ? 'var(--state-ok)' : d.result === 'suppressed' ? 'var(--text-secondary)' : 'var(--text-muted)' }}>
                    {d.result === 'announced' ? 'Said' : d.result === 'suppressed' ? 'Quiet' : 'Note'}
                  </span>
                  <span>
                    {d.device && <strong>{d.device}: </strong>}
                    {d.why}
                  </span>
                </li>
              ))}
            </ul>
          ))}
      </Section>
    </div>
  );
}
