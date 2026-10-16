import { useCallback, useEffect, useState, type CSSProperties } from 'react';
import { Play, Pause, Trash2, CalendarClock, Bell, Check } from 'lucide-react';
import { flowsApi, type CampaignView, type Misfire, type NotifyPrefs, type Occurrence, type Notification, type ScheduleBody, type ScheduleView } from '../api/flows';
import { Pill } from './FlowRun';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 14 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const button: CSSProperties = { padding: '6px 11px', borderRadius: 8, border: '1px solid rgba(6,182,212,0.4)', background: 'rgba(6,182,212,0.12)', color: 'var(--accent-cyan)', fontSize: 12.5, cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 6 };
const dangerButton: CSSProperties = { ...button, border: '1px solid rgba(248,113,113,0.5)', background: 'rgba(248,113,113,0.12)', color: '#f87171' };
const input: CSSProperties = { width: '100%', background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: '6px 8px', color: 'inherit', fontSize: 13, boxSizing: 'border-box' };

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
const when = (t: number) => new Date(t * 1000).toLocaleString();

const STATE_TEXT: Record<ScheduleView['state'], string> = {
  paused: 'switched off', scheduled: 'scheduled', running: 'running', needs_attention: 'needs you', completed: 'done', invalid: 'the flow has problems',
};
const STATE_COLOUR: Record<ScheduleView['state'], string> = {
  paused: 'paused', scheduled: 'queued', running: 'running', needs_attention: 'needs_action', completed: 'succeeded', invalid: 'failed',
};

const ZONES = ['', 'UTC', 'Asia/Kolkata', 'America/New_York', 'America/Los_Angeles', 'Europe/London', 'Europe/Berlin', 'Asia/Dubai', 'Asia/Singapore', 'Asia/Tokyo', 'Australia/Sydney'];

const MISFIRE_TEXT: Record<Misfire, string> = {
  ask: 'Ask me what to do',
  run_now: 'Run it as soon as Aqua is back',
  skip: 'Skip it',
  next_run: 'Wait for the next scheduled time',
};

const NOTIFY_ROWS: [keyof NotifyPrefs, string][] = [
  ['on_start', 'When it starts'], ['on_success', 'When it succeeds'], ['on_failure', 'When it fails'], ['on_recovered', 'When it recovered by itself'], ['on_needs_you', 'When it needs me'],
];

/** One line for an occurrence: what happened to a time the schedule came due. */
function outcomeText(o: Occurrence): string {
  const words: Record<string, string> = {
    started: 'ran', started_late: 'ran late', skipped: 'skipped', missed_pending: 'missed, waiting for your decision', failed_to_start: 'could not start', resolved_run: 'missed, then run on your say', resolved_skip: 'missed, skipped on your say',
  };
  return `${when(o.due_at)}: ${words[o.outcome] ?? o.outcome}${o.reason ? ` (${o.reason})` : ''}`;
}

export function useSchedules(flowId?: number) {
  const [items, setItems] = useState<ScheduleView[]>([]);
  const [error, setError] = useState('');
  const reload = useCallback(() => flowsApi.schedules(flowId).then((r) => { setItems(r.schedules); setError(''); }).catch((e) => setError(message(e))), [flowId]);
  useEffect(() => {
    void reload();
    const t = window.setInterval(() => void reload(), 5000);
    return () => window.clearInterval(t);
  }, [reload]);
  return { items, error, reload, setError };
}

function ScheduleRow({ s, onChanged, onError, onOpenRun }: { s: ScheduleView; onChanged: () => void; onError: (e: string) => void; onOpenRun?: (flowId: number, runId: number) => void }) {
  const [history, setHistory] = useState<Occurrence[] | null>(null);
  const [camp, setCamp] = useState<CampaignView | null>(null);
  const act = (p: Promise<unknown>) => p.then(onChanged).catch((e) => onError(message(e)));
  return (
    <div style={{ ...card, display: 'grid', gap: 6 }} data-testid="schedule-row">
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
        <strong style={{ flex: 1, minWidth: 120 }}>{s.name}</strong>
        <Pill status={STATE_COLOUR[s.state]} text={STATE_TEXT[s.state]} />
      </div>
      <div style={muted}>{s.flow_name} · {s.when.type === 'cron' ? `cron ${s.when.cron}` : `once ${s.when.when}`} · {s.zone_label}</div>
      {(s.campaign?.max_runs || s.campaign?.end_on) && (
        <div style={muted} data-testid="campaign-line">Campaign: {s.runs_started}{s.campaign.max_runs ? ` of ${s.campaign.max_runs}` : ''} run{s.runs_started === 1 ? '' : 's'} started{s.campaign.end_on ? ` · last day ${s.campaign.end_on}` : ''}{s.campaign.series && s.campaign.series.length > 0 ? ` · ${s.campaign.series.length} inputs take turns` : ''}</div>
      )}
      <div style={muted}>{s.enabled && s.next_run_text ? `Next: ${s.next_run_text}` : s.enabled ? 'Nothing more to run' : 'Not running'}{s.last_run_at ? ` · Last: ${s.last_status || 'ran'} (${when(s.last_run_at)})` : ''}</div>
      {s.pending_missed !== null && (
        <div style={{ ...card, borderColor: 'rgba(245,158,11,0.55)', display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }} data-testid="missed-card">
          <span style={{ flex: 1 }}>A run was missed while Aqua was not running. Run it now?</span>
          <button style={button} onClick={() => act(flowsApi.resolveMissed(s.pending_missed!, 'run'))}>Run it now</button>
          <button style={button} onClick={() => act(flowsApi.resolveMissed(s.pending_missed!, 'skip'))}>Skip it</button>
        </div>
      )}
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        <button style={button} onClick={() => act(flowsApi.scheduleAction(s.id, 'run-now'))} disabled={s.active_run !== null}><Play size={13} /> Run now</button>
        {s.enabled
          ? <button style={button} onClick={() => act(flowsApi.scheduleAction(s.id, 'disable'))}><Pause size={13} /> Switch off</button>
          : <button style={button} onClick={() => act(flowsApi.scheduleAction(s.id, 'enable'))}>Switch on</button>}
        {s.active_run !== null && onOpenRun && <button style={button} onClick={() => onOpenRun(s.flow_id, s.active_run!)}>Watch the run</button>}
        {s.last_run_id !== null && onOpenRun && s.active_run === null && <button style={button} onClick={() => onOpenRun(s.flow_id, s.last_run_id!)}>Last run</button>}
        <button style={button} onClick={() => (history ? (setHistory(null), setCamp(null)) : void Promise.all([flowsApi.occurrences(s.id), flowsApi.campaign(s.id)]).then(([o, c]) => { setHistory(o.occurrences); setCamp(c.campaign); }).catch((e) => onError(message(e))))}>{history ? 'Hide history' : 'History'}</button>
        <button style={dangerButton} onClick={() => act(flowsApi.deleteSchedule(s.id))}><Trash2 size={13} /> Delete</button>
      </div>
      {history && camp && (s.campaign?.max_runs || s.campaign?.end_on) && <div style={muted} data-testid="campaign-summary">So far: {camp.text}</div>}
      {history && (
        <ul style={{ margin: 0, paddingLeft: 18, ...muted }}>
          {history.length === 0 ? <li>Nothing has come due yet.</li> : history.map((o) => <li key={o.id}>{outcomeText(o)}</li>)}
        </ul>
      )}
    </div>
  );
}

/** The Schedule tab of the editor: this flow's schedules, and a form to add one. */
export function SchedulePanel({ flowId, onOpenRun }: { flowId: number; onOpenRun: (runId: number) => void }) {
  const { items, error, reload, setError } = useSchedules(flowId);
  const [text, setText] = useState('');
  const [zone, setZone] = useState('');
  const [misfire, setMisfire] = useState<Misfire>('ask');
  const [notify, setNotify] = useState<NotifyPrefs>({ on_start: true, on_success: true, on_failure: true, on_recovered: true, on_needs_you: true, on_each_step: false, channels: ['in_app'] });
  const [maxRuns, setMaxRuns] = useState('');
  const [endOn, setEndOn] = useState('');
  const [seriesText, setSeriesText] = useState('');
  const [preview, setPreview] = useState<string[] | null>(null);
  const [previewNote, setPreviewNote] = useState('');
  const body = (): ScheduleBody => {
    const campaign: NonNullable<ScheduleBody['campaign']> = {};
    if (maxRuns.trim()) campaign.max_runs = Number(maxRuns);
    if (endOn.trim()) campaign.end_on = endOn.trim();
    if (seriesText.trim()) {
      const parsed = JSON.parse(seriesText);
      if (!Array.isArray(parsed) || parsed.some((x) => typeof x !== 'object' || x === null || Array.isArray(x))) throw new Error('The inputs that take turns must be a JSON list of objects, for example [{"topic": "agents"}, {"topic": "memory"}]');
      campaign.series = parsed;
    }
    return { text, timezone: zone, misfire, notify, campaign };
  };
  const safeBody = (): ScheduleBody | null => { try { return body(); } catch (e) { setError(e instanceof SyntaxError ? 'The inputs that take turns are not valid JSON.' : message(e)); return null; } };

  const showPreview = () => {
    setError('');
    const b = safeBody();
    if (!b) return;
    flowsApi.previewSchedule(b).then((r) => { setPreview(r.next.map((n) => n.text)); setPreviewNote(`${r.summary ?? ''} (${r.zone})`); }).catch((e) => { setPreview(null); setError(message(e)); });
  };
  const create = () => {
    setError('');
    const b = safeBody();
    if (!b) return;
    flowsApi.createSchedule(flowId, b).then(() => { setText(''); setMaxRuns(''); setEndOn(''); setSeriesText(''); setPreview(null); void reload(); }).catch((e) => setError(message(e)));
  };

  return (
    <div style={{ display: 'grid', gap: 10 }} data-testid="schedule-panel">
      <strong><CalendarClock size={14} /> Run this flow by itself</strong>
      <input style={input} value={text} onChange={(e) => { setText(e.target.value); setPreview(null); }} aria-label="When should it run" placeholder="every day at 10 AM · every Monday at 9:30 · tomorrow at 3 pm" />
      <label style={muted}>Time zone
        <select style={{ ...input, marginTop: 4 }} value={zone} onChange={(e) => { setZone(e.target.value); setPreview(null); }} aria-label="Time zone">
          {ZONES.map((z) => <option key={z} value={z}>{z || 'This computer'}</option>)}
        </select>
      </label>
      <label style={muted}>If Aqua is closed when it is due
        <select style={{ ...input, marginTop: 4 }} value={misfire} onChange={(e) => setMisfire(e.target.value as Misfire)} aria-label="If a time is missed">
          {(Object.keys(MISFIRE_TEXT) as Misfire[]).map((m) => <option key={m} value={m}>{MISFIRE_TEXT[m]}</option>)}
        </select>
      </label>
      <div style={{ display: 'grid', gap: 6 }} data-testid="campaign-fields">
        <span style={muted}>Make it a campaign (optional): it stops by itself. You can also write it above: "for 4 weeks", "8 times".</span>
        <div style={{ display: 'flex', gap: 8 }}>
          <input style={input} type="number" min={1} max={1000} value={maxRuns} onChange={(e) => { setMaxRuns(e.target.value); setPreview(null); }} aria-label="Stop after this many runs" placeholder="Stop after … runs" />
          <input style={input} type="date" value={endOn} onChange={(e) => { setEndOn(e.target.value); setPreview(null); }} aria-label="Last day" />
        </div>
        <textarea style={{ ...input, minHeight: 44 }} value={seriesText} onChange={(e) => setSeriesText(e.target.value)} aria-label="Inputs that take turns" placeholder='Different inputs each run, in turn: [{"topic": "agents"}, {"topic": "memory"}]' />
      </div>
      <div style={{ display: 'grid', gap: 3 }}>
        <span style={muted}>Tell me</span>
        {NOTIFY_ROWS.map(([k, label]) => (
          <label key={String(k)} style={{ fontSize: 12.5, display: 'flex', gap: 6, alignItems: 'center' }}>
            <input type="checkbox" checked={notify[k] as boolean} onChange={(e) => setNotify({ ...notify, [k]: e.target.checked })} /> {label}
          </label>
        ))}
        <span style={muted}>Notices appear in Aqua. Email, Slack and push need an account connected first.</span>
      </div>
      <div style={{ display: 'flex', gap: 6 }}>
        <button style={button} onClick={showPreview} disabled={!text.trim()}>Show next times</button>
        <button style={button} onClick={create} disabled={!text.trim()} data-testid="schedule-create">Schedule it</button>
      </div>
      {preview && (
        <div style={muted} data-testid="schedule-preview">{previewNote}<ol style={{ margin: '4px 0 0', paddingLeft: 18 }}>{preview.map((p) => <li key={p}>{p}</li>)}</ol></div>
      )}
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{error}</div>}
      {items.map((s) => <ScheduleRow key={s.id} s={s} onChanged={() => void reload()} onError={setError} onOpenRun={(_, r) => onOpenRun(r)} />)}
    </div>
  );
}

/** Every scheduled flow, on the list page: what is due next, what needs a decision. */
export function ScheduledDashboard({ onOpen }: { onOpen: (flowId: number, runId: number) => void }) {
  const { items, error, reload, setError } = useSchedules();
  if (items.length === 0 && !error) return null;
  return (
    <div style={{ display: 'grid', gap: 8 }} data-testid="schedule-dashboard">
      <strong><CalendarClock size={14} /> Scheduled flows ({items.length})</strong>
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{error}</div>}
      {items.map((s) => <ScheduleRow key={s.id} s={s} onChanged={() => void reload()} onError={setError} onOpenRun={onOpen} />)}
    </div>
  );
}

const LEVEL_COLOUR: Record<Notification['level'], string> = { info: 'running', success: 'succeeded', warning: 'waiting', error: 'failed' };

/** What Aqua has told you: runs that started, finished, failed or need you. */
export function Notifications({ onOpen }: { onOpen: (flowId: number, runId: number) => void }) {
  const [list, setList] = useState<Notification[]>([]);
  const [unread, setUnread] = useState(0);
  const [open, setOpen] = useState(false);
  const load = useCallback(() => flowsApi.notifications().then((r) => { setList(r.notifications); setUnread(r.unread); }).catch(() => undefined), []);
  useEffect(() => {
    void load();
    const t = window.setInterval(() => void load(), 5000);
    return () => window.clearInterval(t);
  }, [load]);
  const readAll = () => flowsApi.markRead(list.filter((n) => !n.read).map((n) => n.id)).then(load).catch(() => undefined);
  return (
    <div style={{ display: 'grid', gap: 8 }}>
      <div>
        <button style={button} onClick={() => setOpen(!open)} data-testid="notifications-toggle" aria-expanded={open}>
          <Bell size={14} /> Notifications{unread > 0 && <span data-testid="unread-count" style={{ background: '#f87171', color: '#fff', borderRadius: 999, padding: '0 7px', fontSize: 11 }}>{unread}</span>}
        </button>
      </div>
      {open && (
        <div style={{ ...card, display: 'grid', gap: 8 }} data-testid="notifications">
          <div style={{ display: 'flex', alignItems: 'center' }}>
            <strong style={{ flex: 1 }}>{list.length === 0 ? 'Nothing yet' : 'Recent'}</strong>
            {unread > 0 && <button style={button} onClick={() => void readAll()}><Check size={13} /> Mark all read</button>}
          </div>
          {list.map((n) => (
            <div key={n.id} style={{ display: 'flex', gap: 10, alignItems: 'flex-start', opacity: n.read ? 0.65 : 1 }}>
              <Pill status={LEVEL_COLOUR[n.level]} text={n.kind.replace('_', ' ')} />
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontSize: 13 }}>{n.title}</div>
                <div style={{ ...muted, whiteSpace: 'pre-wrap' }}>{n.body}</div>
                <div style={muted}>{when(n.at)}{n.delivery.filter((d) => !d.ok).map((d) => ` · ${d.channel}: not delivered`).join('')}</div>
              </div>
              {n.run_id !== null && <button style={button} onClick={() => onOpen(n.flow_id, n.run_id!)}>Open</button>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
