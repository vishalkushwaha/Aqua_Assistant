import { useState } from 'react';
import { orbPlacement, useOrbPlacement, type OrbLogEntry, type OrbPlacementSettings, type Sensitivity, type SpotName } from '../api/orbPlacement';
import { PageHeader, Pill, Section, Switch, UnavailableState } from './ui';

const SELECT_STYLE = {
  padding: '5px 8px',
  fontSize: 13,
  borderRadius: 8,
  border: '1px solid var(--glass-border)',
  background: 'rgba(0,0,0,0.3)',
  color: 'var(--text-primary)',
  outline: 'none',
} as const;

const SENSITIVITY_HINT: Record<Sensitivity, string> = {
  low: 'Moves the orb only when it is well and truly in the way.',
  medium: 'Moves the orb when about a quarter of it sits on text, buttons or pictures.',
  high: 'Moves the orb as soon as it covers a little content.',
};

const GRID: (SpotName | null)[][] = [
  ['top-left', 'top-center', 'top-right'],
  ['middle-left', null, 'middle-right'],
  ['bottom-left', 'bottom-center', 'bottom-right'],
];

const SPOT_TEXT: Record<string, string> = {
  'top-left': 'Top left',
  'top-center': 'Top',
  'top-right': 'Top right',
  'middle-left': 'Left',
  'middle-right': 'Right',
  'bottom-left': 'Bottom left',
  'bottom-center': 'Bottom',
  'bottom-right': 'Bottom right',
  middle: 'Middle',
};
const spotText = (s: string) => SPOT_TEXT[s] ?? s;

function ago(unix: number): string {
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 5) return 'just now';
  if (s < 90) return `${Math.round(s)} s ago`;
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  return `${Math.round(s / 3600)} h ago`;
}

const KIND_TEXT: Record<OrbLogEntry['kind'], { word: string; colour: string }> = {
  moved: { word: 'Moved', colour: 'var(--state-ok)' },
  kept: { word: 'Stayed', colour: 'var(--text-secondary)' },
  saved: { word: 'Saved', colour: 'var(--accent-cyan)' },
  skipped: { word: 'Skipped', colour: 'var(--text-muted)' },
  setting: { word: 'Setting', colour: 'var(--text-muted)' },
  forgot: { word: 'Forgot', colour: 'var(--text-muted)' },
  error: { word: 'Problem', colour: 'var(--state-problem)' },
};

/** The "Orb position" page: how Aqua keeps the orb out of the way of what you are working on. */
export function OrbPositionPanel() {
  const { state, error, setState, refresh } = useOrbPlacement();
  const [message, setMessage] = useState<string | null>(null);
  const [confirmForget, setConfirmForget] = useState(false);

  if (!orbPlacement.available()) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 1000, margin: '0 auto' }}>
        <PageHeader title="Display Settings" subtitle="Position and behavior controls for Aqua's floating assistant orb." />
        <UnavailableState
          title="Open Display Settings in Aqua Desktop"
          description="The desktop app owns the orb window and provides automatic placement, per-app memory, screen-edge controls, and privacy-safe content avoidance."
          status="Desktop-only controls"
        />
      </div>
    );
  }
  if (!state) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 1000, margin: '0 auto' }}>
        <PageHeader title="Display Settings" subtitle={error ?? 'Loading…'} />
      </div>
    );
  }

  const s = state.settings;
  const set = (patch: Partial<OrbPlacementSettings>) => {
    orbPlacement.set(patch).then(
      (next) => {
        setState(next);
        setMessage(null);
      },
      (e: unknown) => setMessage(e instanceof Error ? e.message : String(e)),
    );
  };
  const moveTo = (spot: SpotName) => {
    void orbPlacement.move(spot).then(() => window.setTimeout(refresh, 700));
  };
  const forget = (app?: string) => {
    orbPlacement.forget(app).then(setState, (e: unknown) => setMessage(e instanceof Error ? e.message : String(e)));
    setConfirmForget(false);
  };
  const c = state.counters;
  const moving = s.auto_move && !s.lock;
  const cpuSeconds = (state.cpu_ms.worker + state.cpu_ms.hooks) / 1000;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 1000, margin: '0 auto' }}>
      <PageHeader
        title="Display Settings"
        subtitle="When you switch program or page, Aqua looks once at the window in front and, only if the orb is covering something, slides it to the clearest nearby spot. It never moves while you drag it, while its text box is open, or when you lock it. The picture of the window is analysed and thrown away; nothing is stored about what is on your screen."
        actions={
          <>
            <button className="btn-ghost" onClick={() => void orbPlacement.recheck().then(() => window.setTimeout(refresh, 900))}>
              Check now
            </button>
            <button className="btn-primary" onClick={() => set({ auto_move: !s.auto_move })}>
              {s.auto_move ? 'Turn off automatic moving' : 'Turn on automatic moving'}
            </button>
          </>
        }
      />

      {(message || error) && (
        <div role="alert" className="glass-panel" style={{ padding: 14, color: 'var(--state-problem)' }}>
          {message ?? error}
        </div>
      )}
      {state.status.note && (
        <div className="glass-panel" style={{ padding: 14, color: 'var(--state-attention)' }}>
          {state.status.note}
        </div>
      )}
      {!moving && (
        <div className="glass-panel" style={{ padding: 14, color: 'var(--text-secondary)', fontSize: 14 }}>
          {s.lock ? 'The orb is locked: it stays where it is until you move it yourself or unlock it.' : 'Automatic moving is off: the orb stays where you put it.'}
        </div>
      )}

      <Section title="How the orb behaves">
        <Switch label="Move out of the way automatically" hint="When the window or page in front changes and the orb is covering something." checked={s.auto_move} onChange={(v) => set({ auto_move: v })} />
        <Switch label="Lock the orb where it is" hint="Nothing moves it except you. You can still drag it; the new spot is kept." checked={s.lock} onChange={(v) => set({ lock: v })} />
        <Switch label="Keep clear of text, buttons and pictures" hint="Judged from one small picture of the window in front, taken when something changes. Turn this off to ignore content and only keep the orb on screen." checked={s.avoid_content} onChange={(v) => set({ avoid_content: v })} disabled={!moving} />
        <Switch label="Keep away from the mouse pointer" hint="Used only to choose between spots, never to move the orb by itself." checked={s.avoid_cursor} onChange={(v) => set({ avoid_cursor: v })} disabled={!moving} />
        <Switch label="Move the orb when I scroll" hint="Text that scrolls under the orb is noticed through the mouse wheel. Only wheel turns are listened to (nothing else you do with the mouse or keyboard), and with this switch off no mouse listener exists. Keyboard and scroll-bar scrolling are noticed at the next change of window or page, or with Check now." checked={s.scroll_move} onChange={(v) => set({ scroll_move: v })} disabled={!moving} />
        <Switch label="Also while I am still scrolling" hint="Looks at the area around the orb every moment or so while the wheel turns, not only once it stops. Off: only after you stop." checked={s.scroll_live} onChange={(v) => set({ scroll_live: v })} disabled={!moving || !s.scroll_move} />
        <label style={{ display: 'flex', gap: 10, alignItems: 'center', fontSize: 14, flexWrap: 'wrap', opacity: moving && s.scroll_move && s.scroll_live ? 1 : 0.5 }}>
          How quickly it reacts while I scroll
          <select style={SELECT_STYLE} value={s.scroll_interval_ms} disabled={!moving || !s.scroll_move || !s.scroll_live} onChange={(e) => set({ scroll_interval_ms: Number(e.target.value) })}>
            <option value={700}>Gently (about every 0.7 s)</option>
            <option value={350}>Normally (about every 0.35 s)</option>
            <option value={200}>Quickly (about every 0.2 s)</option>
            {![700, 350, 200].includes(s.scroll_interval_ms) && <option value={s.scroll_interval_ms}>Every {s.scroll_interval_ms} ms</option>}
          </select>
          <span style={{ fontSize: 13, color: 'var(--text-secondary)' }}>Quicker costs more processor time while you scroll; nothing while you do not.</span>
        </label>
        <Switch label="Remember a spot for each program" hint="When you drag the orb while a program is in front, Aqua goes back to that spot in that program whenever it is clear." checked={s.per_app} onChange={(v) => set({ per_app: v })} />
        <Switch label="Remember a spot for each page or document" hint="Keyed by a one-way fingerprint of the window's title, never the title itself. At most 100 are kept." checked={s.per_page} onChange={(v) => set({ per_page: v })} />
        <label style={{ display: 'flex', gap: 10, alignItems: 'center', fontSize: 14, flexWrap: 'wrap', opacity: moving ? 1 : 0.5 }}>
          How easily the orb moves
          <select style={SELECT_STYLE} value={s.sensitivity} disabled={!moving} onChange={(e) => set({ sensitivity: e.target.value as Sensitivity })}>
            <option value="low">Rarely</option>
            <option value="medium">Normally</option>
            <option value="high">Readily</option>
          </select>
          <span style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{SENSITIVITY_HINT[s.sensitivity]}</span>
        </label>
      </Section>

      <Section
        title="Put the orb somewhere"
        hint={state.preferred ? `Your preferred spot is ${spotText(state.preferred).toLowerCase()}. Dragging the orb changes it.` : 'You have not chosen a spot yet, so the orb returns to the bottom right corner whenever that is clear. Dragging the orb, or picking one here, sets your own.'}
      >
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, minmax(0, 120px))', gap: 8, alignSelf: 'flex-start' }} role="group" aria-label="Move the orb to">
          {GRID.flat().map((spot, i) =>
            spot ? (
              <button key={spot} className="btn-ghost" onClick={() => moveTo(spot)} style={{ padding: '10px 8px', border: `1px solid ${state.preferred === spot ? 'var(--accent-cyan)' : 'var(--glass-border)'}`, borderRadius: 8 }} aria-pressed={state.preferred === spot}>
                {spotText(spot)}
              </button>
            ) : (
              <span key={i} aria-hidden style={{ border: '1px dashed var(--glass-border)', borderRadius: 8, display: 'grid', placeItems: 'center', fontSize: 12, color: 'var(--text-muted)' }}>
                screen
              </span>
            ),
          )}
        </div>
        <div>
          <button className="btn-ghost" onClick={() => moveTo('default')}>
            Back to the default corner
          </button>
        </div>
      </Section>

      <Section
        title="Remembered spots"
        hint="Where you have put the orb. A remembered spot is only used when it is clear in the window in front."
        actions={
          !confirmForget ? (
            <button className="btn-ghost" disabled={!state.preferred && state.apps.length === 0 && state.pages === 0} onClick={() => setConfirmForget(true)}>
              Forget all
            </button>
          ) : (
            <span style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13.5 }}>
              Forget every remembered spot?
              <button className="btn-danger solid" onClick={() => forget()}>Yes, forget</button>
              <button className="btn-ghost" onClick={() => setConfirmForget(false)}>Keep</button>
            </span>
          )
        }
      >
        <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
          <Pill tone={state.preferred ? 'ok' : 'neutral'} label="Preferred spot" value={state.preferred ? spotText(state.preferred) : 'none (default corner)'} />
          <Pill label="Programs" value={state.apps.length} />
          <Pill label="Pages" value={state.pages} />
        </div>
        {state.apps.length === 0 ? (
          <p style={{ fontSize: 14, color: 'var(--text-secondary)' }}>{s.per_app ? 'No program has its own spot yet. Drag the orb while a program is in front and it will be remembered for that program.' : 'Remembering a spot per program is switched off.'}</p>
        ) : (
          <ul style={{ listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 6 }}>
            {state.apps.map((a) => (
              <li key={a.app} style={{ display: 'flex', gap: 12, alignItems: 'center', fontSize: 14 }}>
                <strong style={{ minWidth: 170 }}>{a.app}</strong>
                <span style={{ color: 'var(--text-secondary)', minWidth: 110 }}>{spotText(a.spot)}</span>
                <button className="btn-ghost" style={{ padding: '3px 10px' }} onClick={() => forget(a.app)}>
                  Forget
                </button>
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="What Aqua is doing" hint="Why the orb moved, and why it stayed where it was.">
        <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
          <Pill tone="ok" label="Moved" value={c.moves} />
          <Pill label="Stayed" value={c.kept} />
          <Pill label="Windows looked at" value={c.captures} title="How many times a picture of a window was taken and analysed." />
          <Pill label="Changes noticed" value={c.events} title="Window events that reached Aqua." />
          <Pill label="Scroll looks" value={c.scroll_checks} title={`Times the window was looked at because you scrolled; ${c.scroll_wide} of them had to look at the whole window.`} />
          <Pill label="Effort so far" value={`${cpuSeconds.toFixed(2)} s of processor time`} title={`Since Aqua started ${Math.round(state.uptime_s / 60)} minutes ago.`} />
        </div>
        <ul style={{ listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 4, fontSize: 13.5, color: 'var(--text-secondary)' }}>
          <li>Noticing changes: {state.status.detection || 'starting…'}</li>
          <li>Program in front: {state.status.app || 'none looked at yet'}{state.status.fullscreen ? ' (full screen)' : ''}</li>
          <li>Last look at a window: {state.status.capture ? `${state.status.capture}${state.status.capture_ms ? `, ${state.status.capture_ms} ms` : ''}` : 'none yet'}</li>
          <li>Last time it did not look at a window: {state.status.last_skip ? `${state.status.last_skip.text} (${ago(state.status.last_skip.t)})` : 'never'}</li>
          <li>The orb: {state.status.orb ? (state.status.orb.visible ? 'showing' : 'hidden') : 'window not found'}</li>
        </ul>
        {state.log.length === 0 ? (
          <p style={{ fontSize: 14, color: 'var(--text-secondary)' }}>Nothing yet. Switch to another program or page and come back here.</p>
        ) : (
          <ul style={{ listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 6, fontSize: 13.5 }}>
            {state.log.slice(0, 20).map((e, i) => (
              <li key={`${e.t}-${i}`} style={{ display: 'flex', gap: 10, alignItems: 'baseline' }}>
                <span style={{ color: 'var(--text-muted)', minWidth: 70 }}>{ago(e.t)}</span>
                <span style={{ color: KIND_TEXT[e.kind].colour, minWidth: 56 }}>{KIND_TEXT[e.kind].word}</span>
                <span>{e.text}</span>
              </li>
            ))}
          </ul>
        )}
      </Section>
    </div>
  );
}
