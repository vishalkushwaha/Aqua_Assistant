import { useCallback, useEffect, useState, type CSSProperties, type ReactNode } from 'react';
import { Workflow as WorkflowIcon, Play, Plus, Pencil, Trash2, History, RefreshCw, X, ArrowUp, ArrowDown, Ban } from 'lucide-react';
import { describeTrigger, workflowsApi, type Run, type RunStatus, type SchedulerStatus, type Step, type Trigger, type Workflow, type WorkflowInput } from '../api/workflows';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 16 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const button: CSSProperties = { padding: '7px 12px', borderRadius: 8, border: '1px solid rgba(6,182,212,0.4)', background: 'rgba(6,182,212,0.12)', color: 'var(--accent-cyan)', fontSize: 12.5, cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 6 };
const dangerButton: CSSProperties = { ...button, border: '1px solid rgba(248,113,113,0.5)', background: 'rgba(248,113,113,0.12)', color: '#f87171' };
const input: CSSProperties = { width: '100%', background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: '7px 9px', color: 'inherit', fontSize: 13, boxSizing: 'border-box' };
const mono: CSSProperties = { ...input, fontFamily: 'ui-monospace, Consolas, monospace', fontSize: 12 };

const STATUS_COLOUR: Record<string, string> = {
  running: 'var(--accent-cyan)',
  succeeded: 'var(--accent-emerald)',
  partial: 'var(--accent-amber)',
  failed: 'var(--accent-rose)',
  cancelled: 'var(--text-muted)',
  interrupted: 'var(--accent-amber)',
  skipped: 'var(--text-muted)',
  pending: 'var(--text-muted)',
};

function Badge({ status }: { status: RunStatus | StepStatus | string }) {
  if (!status) return null;
  const c = STATUS_COLOUR[status] ?? '#94a3b8';
  return <span style={{ fontSize: 11.5, padding: '2px 8px', borderRadius: 999, border: `1px solid ${c}66`, color: c, background: `${c}1f` }}>{status}</span>;
}
type StepStatus = 'pending' | 'running' | 'succeeded' | 'failed' | 'skipped';

function Switch({ on, onChange, label }: { on: boolean; onChange: (v: boolean) => void; label: string }) {
  return (
    <button type="button" role="switch" aria-checked={on} aria-label={label} onClick={() => onChange(!on)}
      style={{ width: 44, height: 24, borderRadius: 999, border: '1px solid var(--glass-border)', background: on ? 'rgba(16,185,129,0.2)' : 'rgba(255,255,255,0.05)', position: 'relative', cursor: 'pointer', flexShrink: 0, transition: 'all 0.2s', boxShadow: on ? '0 0 10px rgba(16,185,129,0.2)' : 'none' }}>
      <span style={{ position: 'absolute', top: 2, left: on ? 22 : 2, width: 18, height: 18, borderRadius: '50%', background: on ? 'var(--accent-emerald)' : 'var(--text-muted)', transition: 'left 0.2s cubic-bezier(0.4, 0, 0.2, 1), background 0.2s' }} />
    </button>
  );
}

const when = (t: number | null | undefined) => (t ? new Date(t * 1000).toLocaleString() : '—');
const took = (r: Run) => (r.finished_at && r.started_at ? `${Math.max(0, r.finished_at - r.started_at)}s` : '');

function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label style={{ display: 'block' }}>
      <div style={{ ...muted, marginBottom: 4 }}>{label}</div>
      {children}
      {hint && <div style={{ ...muted, fontSize: 11.5, marginTop: 3 }}>{hint}</div>}
    </label>
  );
}

// ---- editor -----------------------------------------------------------------------------

type TriggerKind = 'daily' | 'weekdays' | 'interval' | 'cron' | 'startup' | 'webhook' | 'manual';
type StepKindForm = 'tool' | 'agent' | 'set';

interface StepForm {
  id: string;
  name: string;
  kind: StepKindForm;
  tool: string;
  argsText: string;
  goal: string;
  maxSteps: number;
  value: string;
  onlyIf: string;
  retries: number;
  retryDelay: number;
  onFailure: 'stop' | 'continue';
}

interface Form {
  name: string;
  description: string;
  enabled: boolean;
  catchUp: boolean;
  triggerKind: TriggerKind;
  time: string;
  intervalMins: number;
  cron: string;
  steps: StepForm[];
}

const blankStep = (n: number, kind: StepKindForm = 'tool'): StepForm => ({ id: `step${n}`, name: '', kind, tool: '', argsText: '{}', goal: '', maxSteps: 8, value: '', onlyIf: '', retries: 0, retryDelay: 5, onFailure: 'stop' });

const blankForm = (): Form => ({ name: '', description: '', enabled: true, catchUp: false, triggerKind: 'daily', time: '09:00', intervalMins: 60, cron: '0 9 * * *', steps: [blankStep(1)] });

function cronOf(f: Form): string {
  if (f.triggerKind === 'cron') return f.cron.trim();
  const [h, m] = f.time.split(':').map((x) => parseInt(x, 10) || 0);
  return `${m} ${h} * * ${f.triggerKind === 'weekdays' ? '1-5' : '*'}`;
}

function formOf(w: Workflow): Form {
  const f = blankForm();
  f.name = w.name;
  f.description = w.description;
  f.enabled = w.enabled;
  f.catchUp = w.catch_up;
  const t = w.trigger;
  if (t.type === 'schedule') {
    const m = /^(\d{1,2}) (\d{1,2}) \* \* (\*|1-5)$/.exec(t.cron.trim());
    if (m) {
      f.triggerKind = m[3] === '*' ? 'daily' : 'weekdays';
      f.time = `${m[2].padStart(2, '0')}:${m[1].padStart(2, '0')}`;
    } else {
      f.triggerKind = 'cron';
      f.cron = t.cron;
    }
  } else if (t.type === 'interval') {
    f.triggerKind = 'interval';
    f.intervalMins = Math.max(1, Math.round(t.every_secs / 60));
  } else {
    f.triggerKind = t.type;
  }
  f.steps = w.steps.map((s) => ({
    ...blankStep(1, s.type),
    id: s.id,
    name: s.name,
    tool: s.type === 'tool' ? s.tool : '',
    argsText: s.type === 'tool' ? JSON.stringify(s.arguments ?? {}, null, 2) : '{}',
    goal: s.type === 'agent' ? s.goal : '',
    maxSteps: s.type === 'agent' ? s.max_steps : 8,
    value: s.type === 'set' ? s.value : '',
    onlyIf: s.only_if ?? '',
    retries: s.retries,
    retryDelay: s.retry_delay_secs,
    onFailure: s.on_failure,
  }));
  return f;
}

/** Turn the form into what the server takes. Throws a plain message for what cannot be sent. */
function inputOf(f: Form): WorkflowInput {
  let trigger: Trigger;
  switch (f.triggerKind) {
    case 'daily':
    case 'weekdays':
    case 'cron':
      trigger = { type: 'schedule', cron: cronOf(f) };
      break;
    case 'interval':
      trigger = { type: 'interval', every_secs: Math.round(f.intervalMins * 60) };
      break;
    default:
      trigger = { type: f.triggerKind };
  }
  const steps: Step[] = f.steps.map((s, i) => {
    const base = { id: s.id.trim(), name: s.name.trim(), only_if: s.onlyIf.trim() || null, retries: s.retries, retry_delay_secs: s.retryDelay, on_failure: s.onFailure, timeout_secs: 0 };
    if (s.kind === 'tool') {
      let args: Record<string, unknown>;
      try {
        const parsed: unknown = JSON.parse(s.argsText.trim() || '{}');
        if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) throw new Error();
        args = parsed as Record<string, unknown>;
      } catch {
        throw new Error(`Step ${i + 1}: the arguments must be a JSON object, for example {"path": "notes.txt"}`);
      }
      return { ...base, type: 'tool', tool: s.tool.trim(), arguments: args };
    }
    if (s.kind === 'agent') return { ...base, type: 'agent', goal: s.goal, max_steps: s.maxSteps };
    return { ...base, type: 'set', value: s.value };
  });
  return { name: f.name.trim(), description: f.description.trim(), enabled: f.enabled, trigger, steps, catch_up: f.catchUp };
}

const TEMPLATE_HELP = 'Use results of earlier steps: {{steps.<id>.output}} (text) or {{steps.<id>.data.<field>}}; the webhook body: {{trigger.payload.<field>}}; {{now.date}}, {{now.time}}. Add | default: text for a fallback.';

function Editor({ existing, onDone }: { existing: Workflow | null; onDone: () => void }) {
  const [form, setForm] = useState<Form>(existing ? formOf(existing) : blankForm());
  const [error, setError] = useState('');
  const [saving, setSaving] = useState(false);
  const [preview, setPreview] = useState<string[] | string>([]);
  const set = (patch: Partial<Form>) => setForm((f) => ({ ...f, ...patch }));
  const setStep = (i: number, patch: Partial<StepForm>) => setForm((f) => ({ ...f, steps: f.steps.map((s, j) => (j === i ? { ...s, ...patch } : s)) }));

  const timed = form.triggerKind === 'daily' || form.triggerKind === 'weekdays' || form.triggerKind === 'cron';
  const cron = cronOf(form);
  useEffect(() => {
    if (!timed) return;
    const t = setTimeout(() => {
      workflowsApi.previewSchedule(cron, 3).then((r) => setPreview(r.runs.map((x) => new Date(x * 1000).toLocaleString()))).catch((e: Error) => setPreview(e.message));
    }, 300);
    return () => clearTimeout(t);
  }, [cron, timed]);

  const move = (i: number, d: number) =>
    setForm((f) => {
      const steps = [...f.steps];
      const j = i + d;
      if (j < 0 || j >= steps.length) return f;
      [steps[i], steps[j]] = [steps[j], steps[i]];
      return { ...f, steps };
    });

  const save = async () => {
    setError('');
    setSaving(true);
    try {
      const body = inputOf(form);
      if (existing) await workflowsApi.update(existing.id, body);
      else await workflowsApi.create(body);
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div style={{ display: 'grid', gap: 14 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <h3 style={{ margin: 0 }}>{existing ? `Edit “${existing.name}”` : 'New workflow'}</h3>
        <button style={button} onClick={onDone}><X size={14} /> Close</button>
      </div>
      <div style={{ ...card, display: 'grid', gap: 12 }}>
        <Field label="Name"><input style={input} value={form.name} onChange={(e) => set({ name: e.target.value })} placeholder="Morning market report" /></Field>
        <Field label="Description (optional)"><input style={input} value={form.description} onChange={(e) => set({ description: e.target.value })} /></Field>
        <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', alignItems: 'end' }}>
          <Field label="When should it run?">
            <select style={{ ...input, width: 230 }} value={form.triggerKind} onChange={(e) => set({ triggerKind: e.target.value as TriggerKind })}>
              <option value="daily">Every day at a time</option>
              <option value="weekdays">Weekdays at a time</option>
              <option value="interval">Every N minutes</option>
              <option value="cron">Custom schedule (cron)</option>
              <option value="startup">Each time Aqua starts</option>
              <option value="webhook">When a webhook calls it</option>
              <option value="manual">Only when I press Run</option>
            </select>
          </Field>
          {(form.triggerKind === 'daily' || form.triggerKind === 'weekdays') && <Field label="Time (your local time)"><input type="time" style={{ ...input, width: 140 }} value={form.time} onChange={(e) => set({ time: e.target.value })} /></Field>}
          {form.triggerKind === 'interval' && <Field label="Every (minutes, at least 1)"><input type="number" min={1} style={{ ...input, width: 140 }} value={form.intervalMins} onChange={(e) => set({ intervalMins: Number(e.target.value) })} /></Field>}
          {form.triggerKind === 'cron' && <Field label="Cron: minute hour day month weekday"><input style={{ ...mono, width: 220 }} value={form.cron} onChange={(e) => set({ cron: e.target.value })} /></Field>}
        </div>
        {timed && <div style={muted}>Next runs: {Array.isArray(preview) ? preview.join('  ·  ') || '…' : <span style={{ color: '#f87171' }}>{preview}</span>}</div>}
        {form.triggerKind === 'webhook' && <div style={muted}>Call <code>POST /api/v1/workflows/&lt;id&gt;/trigger</code> with the API token; the JSON body is available as <code>{'{{trigger.payload}}'}</code>.</div>}
        <div style={{ display: 'flex', gap: 24, flexWrap: 'wrap' }}>
          <label style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13 }}><Switch on={form.enabled} onChange={(v) => set({ enabled: v })} label="Enabled" /> Enabled</label>
          {timed || form.triggerKind === 'interval' ? (
            <label style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13 }} title="If Aqua was closed when a run was due, run it once when Aqua starts again. Otherwise it is skipped.">
              <input type="checkbox" checked={form.catchUp} onChange={(e) => set({ catchUp: e.target.checked })} /> Run once if it was missed while Aqua was closed
            </label>
          ) : null}
        </div>
      </div>

      <div style={{ ...muted }}>{TEMPLATE_HELP}</div>
      {form.steps.map((s, i) => (
        <div key={i} style={{ ...card, display: 'grid', gap: 10 }}>
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
            <strong style={{ fontSize: 13 }}>Step {i + 1}</strong>
            <select style={{ ...input, width: 190 }} value={s.kind} onChange={(e) => setStep(i, { kind: e.target.value as StepKindForm })} aria-label="Step type">
              <option value="tool">Run a tool</option>
              <option value="agent">Ask the Aqua agent</option>
              <option value="set">Compose text</option>
            </select>
            <input style={{ ...input, width: 130 }} value={s.id} onChange={(e) => setStep(i, { id: e.target.value })} aria-label="Step id" title="Used to refer to this step's result: {{steps.<id>.output}}" />
            <input style={{ ...input, width: 200 }} value={s.name} onChange={(e) => setStep(i, { name: e.target.value })} placeholder="Label (optional)" aria-label="Step label" />
            <span style={{ flex: 1 }} />
            <button style={button} onClick={() => move(i, -1)} disabled={i === 0} aria-label="Move up"><ArrowUp size={14} /></button>
            <button style={button} onClick={() => move(i, 1)} disabled={i === form.steps.length - 1} aria-label="Move down"><ArrowDown size={14} /></button>
            <button style={dangerButton} onClick={() => set({ steps: form.steps.filter((_, j) => j !== i) })} disabled={form.steps.length === 1} aria-label="Remove step"><Trash2 size={14} /></button>
          </div>
          {s.kind === 'tool' && (
            <>
              <Field label="Tool" hint="For example fs.read, fs.write, web.get, git.status. The same tool policy and folder limits apply as for the agent."><input style={mono} value={s.tool} onChange={(e) => setStep(i, { tool: e.target.value })} placeholder="fs.read" /></Field>
              <Field label="Arguments (JSON)" hint="Never put passwords or tokens here: they are saved as plain text and the server refuses them."><textarea style={{ ...mono, minHeight: 70 }} value={s.argsText} onChange={(e) => setStep(i, { argsText: e.target.value })} /></Field>
            </>
          )}
          {s.kind === 'agent' && (
            <>
              <Field label="Goal for the agent" hint="Text from earlier steps is passed to the agent as data, not as instructions."><textarea style={{ ...input, minHeight: 70 }} value={s.goal} onChange={(e) => setStep(i, { goal: e.target.value })} placeholder="Find today's Nifty 50 level and summarise the market in two sentences" /></Field>
              <Field label="Most planning rounds (1–20)"><input type="number" min={1} max={20} style={{ ...input, width: 110 }} value={s.maxSteps} onChange={(e) => setStep(i, { maxSteps: Number(e.target.value) })} /></Field>
            </>
          )}
          {s.kind === 'set' && <Field label="Text"><textarea style={{ ...input, minHeight: 60 }} value={s.value} onChange={(e) => setStep(i, { value: e.target.value })} placeholder="Report for {{now.date}}: {{steps.step1.output}}" /></Field>}
          <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap' }}>
            <Field label="Only run if (optional)"><input style={{ ...mono, width: 240 }} value={s.onlyIf} onChange={(e) => setStep(i, { onlyIf: e.target.value })} placeholder="{{steps.step1.ok}}" /></Field>
            <Field label="Retries (0–5)"><input type="number" min={0} max={5} style={{ ...input, width: 90 }} value={s.retries} onChange={(e) => setStep(i, { retries: Number(e.target.value) })} /></Field>
            <Field label="Wait before retry (s)"><input type="number" min={0} style={{ ...input, width: 110 }} value={s.retryDelay} onChange={(e) => setStep(i, { retryDelay: Number(e.target.value) })} /></Field>
            <Field label="If it still fails">
              <select style={{ ...input, width: 190 }} value={s.onFailure} onChange={(e) => setStep(i, { onFailure: e.target.value as 'stop' | 'continue' })}>
                <option value="stop">Stop the workflow</option>
                <option value="continue">Carry on with the next step</option>
              </select>
            </Field>
          </div>
        </div>
      ))}
      <div><button style={button} onClick={() => set({ steps: [...form.steps, blankStep(form.steps.length + 1)] })} disabled={form.steps.length >= 20}><Plus size={14} /> Add step</button></div>
      {error && <div role="alert" style={{ ...card, borderColor: 'rgba(248,113,113,0.5)', color: '#f87171', fontSize: 13 }}>{error}</div>}
      <div style={{ display: 'flex', gap: 10 }}>
        <button style={button} onClick={save} disabled={saving}>{saving ? 'Saving…' : existing ? 'Save changes' : 'Create workflow'}</button>
        <button style={{ ...button, opacity: 0.8 }} onClick={onDone}>Cancel</button>
      </div>
    </div>
  );
}

// ---- history ----------------------------------------------------------------------------

function HistoryView({ workflow, onClose }: { workflow: Workflow; onClose: () => void }) {
  const [runs, setRuns] = useState<Run[]>([]);
  const [open, setOpen] = useState<Run | null>(null);
  const [error, setError] = useState('');

  const load = useCallback(async () => {
    try {
      const { runs } = await workflowsApi.runs(workflow.id, 30);
      setRuns(runs);
      setError('');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [workflow.id]);

  const show = useCallback(async (id: number) => {
    try {
      setOpen(await workflowsApi.getRun(id));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // Keep the newest run and the open one fresh while anything is running.
  const running = runs.some((r) => r.status === 'running') || open?.status === 'running';
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => {
      void load();
      if (open) void show(open.id);
    }, 2500);
    return () => clearInterval(t);
  }, [running, load, show, open]);

  return (
    <div style={{ display: 'grid', gap: 14 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <h3 style={{ margin: 0 }}>History: {workflow.name}</h3>
        <div style={{ display: 'flex', gap: 8 }}>
          <button style={button} onClick={() => void load()}><RefreshCw size={14} /> Refresh</button>
          <button style={button} onClick={onClose}><X size={14} /> Close</button>
        </div>
      </div>
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 13 }}>{error}</div>}
      {runs.length === 0 && <div style={muted}>No runs yet.</div>}
      <div style={{ display: 'grid', gridTemplateColumns: 'minmax(260px, 340px) 1fr', gap: 14, alignItems: 'start' }}>
        <div style={{ display: 'grid', gap: 8 }}>
          {runs.map((r) => (
            <button key={r.id} onClick={() => void show(r.id)} style={{ ...card, textAlign: 'left', cursor: 'pointer', color: 'inherit', padding: 10, borderColor: open?.id === r.id ? 'rgba(6,182,212,0.6)' : undefined }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8 }}><Badge status={r.status} /><span style={muted}>{r.trigger_kind} {took(r)}</span></div>
              <div style={{ ...muted, marginTop: 4 }}>{when(r.started_at)}</div>
            </button>
          ))}
        </div>
        {open && (
          <div style={{ ...card, display: 'grid', gap: 10 }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
              <div><Badge status={open.status} /> <span style={muted}>run #{open.id} · {when(open.started_at)}</span></div>
              {open.status === 'running' && <button style={dangerButton} onClick={() => void workflowsApi.cancelRun(open.id).then(() => show(open.id))}><Ban size={14} /> Cancel</button>}
            </div>
            {open.error && <div style={{ color: open.status === 'skipped' ? 'var(--text-secondary)' : '#f87171', fontSize: 13 }}>{open.error}</div>}
            {open.steps.map((s) => (
              <div key={s.idx} style={{ borderTop: '1px solid var(--glass-border)', paddingTop: 8 }}>
                <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
                  <strong style={{ fontSize: 13 }}>{s.idx + 1}. {s.name || s.step_id}</strong><Badge status={s.status} />
                  {s.attempts > 1 && <span style={muted}>{s.attempts} attempts</span>}
                </div>
                {s.error && <div style={{ color: '#f87171', fontSize: 12.5, marginTop: 4 }}>{s.error}</div>}
                {s.output && <pre style={{ ...mono, margin: '6px 0 0', whiteSpace: 'pre-wrap', maxHeight: 220, overflow: 'auto' }}>{s.output}</pre>}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ---- the panel --------------------------------------------------------------------------

type View = { mode: 'list' } | { mode: 'edit'; workflow: Workflow | null } | { mode: 'history'; workflow: Workflow };

export function WorkflowsPanel() {
  const [workflows, setWorkflows] = useState<Workflow[]>([]);
  const [scheduler, setScheduler] = useState<SchedulerStatus | null>(null);
  const [view, setView] = useState<View>({ mode: 'list' });
  const [error, setError] = useState('');
  const [note, setNote] = useState('');

  const load = useCallback(async () => {
    try {
      const r = await workflowsApi.list();
      setWorkflows(r.workflows);
      setScheduler(r.scheduler);
      setError('');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), 10000);
    return () => clearInterval(t);
  }, [load]);

  const act = async (fn: () => Promise<unknown>, done?: string) => {
    try {
      await fn();
      setNote(done ?? '');
      setError('');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    await load();
  };

  if (view.mode === 'edit') return <Editor existing={view.workflow} onDone={() => { setView({ mode: 'list' }); void load(); }} />;
  if (view.mode === 'history') return <HistoryView workflow={view.workflow} onClose={() => setView({ mode: 'list' })} />;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '24px', maxWidth: '1200px', margin: '0 auto', width: '100%', animation: 'fade-in 0.3s ease-out' }}>
      <div className="glass-panel" style={{ padding: '24px', borderRadius: '16px', position: 'relative', overflow: 'hidden' }}>
        <div style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '4px', background: 'linear-gradient(90deg, var(--accent-cyan), var(--accent-purple))' }} />
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: '16px', flexWrap: 'wrap' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '16px' }}>
            <div style={{ width: '48px', height: '48px', borderRadius: '12px', background: 'rgba(0, 240, 255, 0.1)', display: 'flex', alignItems: 'center', justifyContent: 'center', border: '1px solid rgba(0, 240, 255, 0.2)', boxShadow: '0 0 15px rgba(0, 240, 255, 0.15)' }}>
              <WorkflowIcon size={24} color="var(--accent-cyan)" />
            </div>
            <div>
              <h2 style={{ margin: 0, fontSize: '24px', fontWeight: '700', letterSpacing: '-0.02em', color: 'var(--text-primary)' }}>Workflows</h2>
              <p style={{ margin: '4px 0 0 0', fontSize: '14px', color: 'var(--text-secondary)' }}>
                Automate routines natively. Runs locally on your device while Aqua is active.
                {scheduler ? <span style={{ color: 'var(--accent-emerald)', marginLeft: '8px' }}>• Scheduler Active ({scheduler.active_runs} runs)</span> : <span style={{ color: 'var(--accent-amber)', marginLeft: '8px' }}>• Starting Engine...</span>}
              </p>
            </div>
          </div>
          <button 
            style={{
              padding: '12px 20px', borderRadius: '30px', border: 'none', background: 'linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))',
              color: '#fff', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px', fontWeight: '700', fontSize: '14px',
              boxShadow: '0 4px 15px rgba(192, 132, 252, 0.3)', transition: 'all 0.2s'
            }} 
            onClick={() => setView({ mode: 'edit', workflow: null })}
          >
            <Plus size={16} /> New Workflow
          </button>
        </div>
      </div>

      {error && <div className="glass-panel" style={{ padding: '16px', borderRadius: '12px', background: 'rgba(239, 68, 68, 0.05)', border: '1px solid rgba(239, 68, 68, 0.3)', color: 'var(--accent-rose)', display: 'flex', alignItems: 'center', gap: '10px', fontSize: '14px', animation: 'slide-down 0.2s ease-out' }}><span>{error}</span></div>}
      {note && <div className="glass-panel" style={{ padding: '16px', borderRadius: '12px', background: 'rgba(16, 185, 129, 0.05)', border: '1px solid rgba(16, 185, 129, 0.3)', color: 'var(--accent-emerald)', display: 'flex', alignItems: 'center', gap: '10px', fontSize: '14px', animation: 'slide-down 0.2s ease-out' }}><span>{note}</span></div>}
      
      {workflows.length === 0 && !error && (
        <div className="glass-panel" style={{ padding: '40px', borderRadius: '16px', textAlign: 'center', color: 'var(--text-secondary)', border: '1px dashed rgba(255,255,255,0.1)' }}>
          <WorkflowIcon size={48} style={{ opacity: 0.2, marginBottom: '16px' }} />
          <div style={{ fontSize: '16px', fontWeight: '600', color: 'var(--text-primary)', marginBottom: '8px' }}>No workflows configured</div>
          <div>Create your first workflow to automate repetitive tasks or data retrieval.</div>
        </div>
      )}

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(350px, 1fr))', gap: '20px' }}>
        {workflows.map((w) => (
          <div key={w.id} className="glass-panel" style={{ padding: '24px', borderRadius: '16px', display: 'flex', flexDirection: 'column', gap: '16px', border: w.enabled ? '1px solid rgba(0, 240, 255, 0.2)' : '1px solid var(--glass-border)', boxShadow: w.enabled ? '0 4px 20px rgba(0, 240, 255, 0.05)' : 'none', transition: 'all 0.3s' }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: '12px' }}>
              <div style={{ flex: 1, minWidth: 0 }}>
                <h3 style={{ margin: '0 0 4px 0', fontSize: '18px', fontWeight: '700', color: w.enabled ? 'var(--text-primary)' : 'var(--text-muted)' }}>{w.name}</h3>
                <div style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>{w.description || 'No description provided'}</div>
              </div>
              <Switch on={w.enabled} onChange={(v) => void act(() => workflowsApi.enable(w.id, v))} label={`Enable ${w.name}`} />
            </div>
            
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: '12px', fontSize: '12px' }}>
              <span style={{ padding: '4px 10px', background: 'rgba(0,0,0,0.3)', borderRadius: '6px', color: 'var(--text-primary)' }}>{describeTrigger(w.trigger)}</span>
              <span style={{ padding: '4px 10px', background: 'rgba(0,0,0,0.3)', borderRadius: '6px', color: 'var(--text-primary)' }}>{w.steps.length} Step{w.steps.length !== 1 && 's'}</span>
              {w.last_status && <Badge status={w.last_status} />}
            </div>

            <div style={{ background: 'rgba(0,0,0,0.2)', padding: '12px', borderRadius: '8px', fontSize: '12px', color: 'var(--text-muted)', display: 'flex', flexDirection: 'column', gap: '4px' }}>
              <div style={{ display: 'flex', justifyContent: 'space-between' }}><span>Next Run:</span> <span>{w.enabled ? when(w.next_run_at) : 'Offline'}</span></div>
              <div style={{ display: 'flex', justifyContent: 'space-between' }}><span>Last Run:</span> <span>{when(w.last_run_at || null)}</span></div>
            </div>

            <div style={{ display: 'flex', gap: '8px', flexWrap: 'wrap', marginTop: 'auto', paddingTop: '8px', borderTop: '1px solid var(--glass-border)' }}>
              <button 
                style={{ flex: 1, padding: '8px 12px', borderRadius: '8px', background: 'rgba(16, 185, 129, 0.1)', color: 'var(--accent-emerald)', border: '1px solid rgba(16, 185, 129, 0.3)', cursor: 'pointer', display: 'flex', alignItems: 'center', justifyContent: 'center', gap: '6px', fontWeight: '600', fontSize: '13px', transition: 'all 0.2s' }} 
                onClick={() => void act(() => workflowsApi.run(w.id), `Started “${w.name}”. See History.`)}
              >
                <Play size={14} /> Run
              </button>
              <button 
                style={{ padding: '8px 12px', borderRadius: '8px', background: 'rgba(255,255,255,0.05)', color: 'var(--text-primary)', border: '1px solid var(--glass-border)', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '6px', fontSize: '13px', transition: 'all 0.2s' }} 
                onClick={() => setView({ mode: 'history', workflow: w })}
              >
                <History size={14} />
              </button>
              <button 
                style={{ padding: '8px 12px', borderRadius: '8px', background: 'rgba(255,255,255,0.05)', color: 'var(--text-primary)', border: '1px solid var(--glass-border)', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '6px', fontSize: '13px', transition: 'all 0.2s' }} 
                onClick={() => setView({ mode: 'edit', workflow: w })}
              >
                <Pencil size={14} />
              </button>
              <button 
                style={{ padding: '8px 12px', borderRadius: '8px', background: 'rgba(239, 68, 68, 0.05)', color: 'var(--accent-rose)', border: '1px solid rgba(239, 68, 68, 0.2)', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '6px', fontSize: '13px', transition: 'all 0.2s' }} 
                onClick={() => { if (window.confirm(`Delete “${w.name}” and its history?`)) void act(() => workflowsApi.remove(w.id)); }}
              >
                <Trash2 size={14} />
              </button>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
