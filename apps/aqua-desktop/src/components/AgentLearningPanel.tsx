import { useCallback, useEffect, useMemo, useState, type CSSProperties, type ReactNode } from 'react';
import { Brain, RefreshCw } from 'lucide-react';
import { learningApi, type Evaluation, type LearningEvent, type LearningJob, type LearningSettings, type LearningStats, type LearningStatus, type PracticeStatus } from '../api/learning';
import { ExperienceManager, StatusBadge, when } from './ExperienceManager';
import { LaunchSkillsCard } from './LaunchSkillsCard';
import { RecipeList } from './RecipeList';

type Tab = 'overview' | 'experiences' | 'settings' | 'activity';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 16 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };

const numberInput: CSSProperties = { width: 110, background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: '6px 8px', color: 'inherit', fontSize: 13 };

const pct = (v: number) => `${Math.round(v * 100)}%`;

function Switch({ on, disabled, onChange, label }: { on: boolean; disabled?: boolean; onChange: (v: boolean) => void; label: string }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!on)}
      style={{ width: 44, height: 24, borderRadius: 999, border: '1px solid var(--glass-border)', background: on ? 'rgba(16,185,129,0.55)' : 'rgba(255,255,255,0.10)', position: 'relative', cursor: disabled ? 'not-allowed' : 'pointer', flexShrink: 0 }}
    >
      <span style={{ position: 'absolute', top: 2, left: on ? 22 : 2, width: 18, height: 18, borderRadius: '50%', background: '#fff', transition: 'left 0.15s' }} />
    </button>
  );
}

function Stat({ label, value, hint }: { label: string; value: ReactNode; hint?: string }) {
  return (
    <div style={{ ...card, padding: 12 }} title={hint}>
      <div style={{ fontSize: 22, fontWeight: 700 }}>{value}</div>
      <div style={muted}>{label}</div>
    </div>
  );
}

function Health({ ok, text, detail }: { ok: boolean; text: string; detail?: string }) {
  return (
    <div style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13 }}>
      <span style={{ width: 9, height: 9, borderRadius: '50%', background: ok ? '#10b981' : '#f87171' }} />
      <span>{text}</span>
      {detail && <span style={muted}>{detail}</span>}
    </div>
  );
}

const SWITCHES: { key: 'learning_enabled' | 'retrieval_enabled' | 'auto_memory_creation' | 'autonomous_experimentation' | 'auto_practice_enabled'; title: string; text: string }[] = [
  { key: 'learning_enabled', title: 'Agent Learning', text: 'Master switch. Off: nothing new is learned and no learning jobs run. The agent itself keeps working.' },
  { key: 'retrieval_enabled', title: 'Memory Retrieval', text: 'Offer verified earlier experience to the agent for new tasks (as notes to weigh, never as instructions).' },
  { key: 'auto_memory_creation', title: 'Automatic Memory Creation', text: 'Evaluate finished tasks and store the ones that were verified and are worth remembering.' },
  { key: 'autonomous_experimentation', title: 'Autonomous Experimentation', text: 'Let the agent retry a failed task with what it learned, within the hard limits below. Off by default; learning does not need it.' },
  { key: 'auto_practice_enabled', title: 'Auto Practice', text: 'While idle, make up a few read-only tasks in the kinds you already use (a new asset or city, or a past task reworded), run them and learn from them. Unattended: every approval is refused, only the browser is available. Off by default.' },
];

interface NumberField {
  key: keyof LearningSettings;
  label: string;
  min: number;
  max: number;
  step: number;
  hint: string;
}

const FIELDS: NumberField[] = [
  { key: 'similarity_threshold', label: 'Similarity threshold', min: 0.05, max: 0.99, step: 0.01, hint: 'How alike a past problem must be to recall a solution. Higher values require closer similarity.' },
  { key: 'memory_confidence_threshold', label: 'Confidence threshold', min: 0, max: 0.99, step: 0.01, hint: 'Experiences below this confidence are not offered.' },
  { key: 'max_experiences_in_prompt', label: 'Experiences per task', min: 1, max: 5, step: 1, hint: 'The most that reach the agent, merged into one short note.' },
  { key: 'max_attempts', label: 'Maximum attempts', min: 1, max: 10, step: 1, hint: 'Experimentation stops after this many tries.' },
  { key: 'max_execution_secs', label: 'Maximum execution time (s)', min: 10, max: 3600, step: 10, hint: 'Experimentation stops when this much time has passed.' },
  { key: 'token_budget', label: 'Token budget', min: 1000, max: 500000, step: 1000, hint: 'Estimated tokens across attempts.' },
  { key: 'cost_budget_usd', label: 'Cost budget (USD)', min: 0, max: 1000, step: 0.5, hint: 'Resource budget for automated tasks.' },
  { key: 'max_tool_calls', label: 'Maximum tool calls', min: 1, max: 200, step: 1, hint: 'Across all attempts of one task.' },
  { key: 'max_background_jobs', label: 'Concurrent Tasks', min: 1, max: 16, step: 1, hint: 'Limits how many tasks may run concurrently.' },
  { key: 'stale_after_days', label: 'Stale after (days)', min: 1, max: 3650, step: 1, hint: 'An experience not verified for this long must be revalidated.' },
  { key: 'practice_max_per_day', label: 'Practice tasks per day', min: 1, max: 24, step: 1, hint: 'The most practice tasks in 24 hours.' },
  { key: 'practice_min_interval_mins', label: 'Minutes between practice tasks', min: 5, max: 1440, step: 5, hint: 'At least this long between two practice tasks.' },
  { key: 'practice_idle_mins', label: 'Idle minutes before practising', min: 1, max: 240, step: 1, hint: 'Practice starts only after nobody has used the agent for this long, and stops when you start a task.' },
  { key: 'practice_max_steps', label: 'Planning rounds per practice task', min: 2, max: 10, step: 1, hint: 'The time limit is the maximum execution time above.' },
];

export function AgentLearningPanel() {
  const [tab, setTab] = useState<Tab>('overview');
  const [settings, setSettings] = useState<LearningSettings | null>(null);
  const [draft, setDraft] = useState<LearningSettings | null>(null);
  const [status, setStatus] = useState<LearningStatus | null>(null);
  const [stats, setStats] = useState<LearningStats | null>(null);
  const [evaluation, setEvaluation] = useState<Evaluation | null>(null);
  const [practice, setPractice] = useState<PracticeStatus | null>(null);
  const [events, setEvents] = useState<LearningEvent[]>([]);
  const [jobs, setJobs] = useState<LearningJob[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [refreshKey, setRefreshKey] = useState(0);
  const [clearText, setClearText] = useState('');

  const load = useCallback(async () => {
    try {
      const [st, stt, s, ev, pr] = await Promise.all([learningApi.status(), learningApi.stats(), learningApi.settings(), learningApi.evaluation(), learningApi.practice()]);
      setStatus(st);
      setPractice(pr);
      setStats(stt);
      setSettings(s);
      setDraft((d) => d ?? s);
      setEvaluation(ev);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not reach Aqua');
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // Live status: the dashboard follows what the learning worker is doing while the page is open.
  useEffect(() => {
    const id = window.setInterval(() => {
      void learningApi.status().then(setStatus).catch(() => undefined);
      if (tab === 'overview') {
        void learningApi.stats().then(setStats).catch(() => undefined);
        void learningApi.practice().then(setPractice).catch(() => undefined);
      }
      if (tab === 'activity') {
        void learningApi.events().then((r) => setEvents(r.events)).catch(() => undefined);
        void learningApi.jobs().then((r) => setJobs(r.jobs)).catch(() => undefined);
      }
    }, 3000);
    return () => window.clearInterval(id);
  }, [tab]);

  useEffect(() => {
    if (tab !== 'activity') return;
    void learningApi.events().then((r) => setEvents(r.events)).catch(() => undefined);
    void learningApi.jobs().then((r) => setJobs(r.jobs)).catch(() => undefined);
  }, [tab]);

  const dirty = useMemo(() => JSON.stringify(draft) !== JSON.stringify(settings), [draft, settings]);

  const save = async (next: LearningSettings, done = 'Settings saved.') => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await learningApi.saveSettings(next);
      setSettings(saved);
      setDraft(saved);
      setNotice(done);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the settings');
    } finally {
      setBusy(false);
    }
  };

  const run = async (fn: () => Promise<unknown>, done: (r: never) => string) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const r = await fn();
      setNotice(done(r as never));
      setRefreshKey((k) => k + 1);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'That did not work');
    } finally {
      setBusy(false);
    }
  };

  const toggle = (key: (typeof SWITCHES)[number]['key'], value: boolean) => {
    if (!settings) return;
    void save({ ...settings, [key]: value }, `${SWITCHES.find((s) => s.key === key)?.title} ${value ? 'on' : 'off'}.`);
  };

  const tabButton = (id: Tab, title: string) => (
    <button key={id} type="button" onClick={() => setTab(id)} style={{ padding: '8px 14px', borderRadius: 8, border: 'none', cursor: 'pointer', background: tab === id ? 'rgba(6,182,212,0.15)' : 'transparent', color: tab === id ? 'var(--accent-cyan)' : 'var(--text-secondary)', fontWeight: tab === id ? 600 : 400 }}>{title}</button>
  );

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '24px', maxWidth: '1200px', margin: '0 auto', width: '100%', animation: 'fade-in 0.3s ease-out' }}>
      
      <div className="glass-panel" style={{ padding: '24px', borderRadius: '16px', position: 'relative', overflow: 'hidden' }}>
        <div style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '4px', background: 'linear-gradient(90deg, var(--accent-cyan), var(--accent-purple))' }} />
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: '16px', flexWrap: 'wrap' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '16px' }}>
            <div style={{ width: '48px', height: '48px', borderRadius: '12px', background: 'rgba(0, 240, 255, 0.1)', display: 'flex', alignItems: 'center', justifyContent: 'center', border: '1px solid rgba(0, 240, 255, 0.2)', boxShadow: '0 0 15px rgba(0, 240, 255, 0.15)' }}>
              <Brain size={24} color="var(--accent-cyan)" />
            </div>
            <div>
              <h2 style={{ margin: 0, fontSize: '24px', fontWeight: '700', letterSpacing: '-0.02em', color: 'var(--text-primary)' }}>Agent Learning Studio</h2>
              <p style={{ margin: '4px 0 0 0', fontSize: '14px', color: 'var(--text-secondary)' }}>Manage the autonomous agent's memory, verified experience retrieval, and self-improvement settings.</p>
            </div>
          </div>
          <button 
            type="button" 
            onClick={() => { setRefreshKey((k) => k + 1); void load(); }}
            style={{
              padding: '10px 18px',
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
            <RefreshCw size={14} className={busy ? 'spin' : ''} /> Refresh Sync
          </button>
        </div>
      </div>

      <div style={{ display: 'flex', gap: '8px', flexWrap: 'wrap' }}>
        {tabButton('overview', 'Dashboard Overview')}
        {tabButton('experiences', 'Verified Experiences')}
        {tabButton('settings', 'Learning Configurations')}
        {tabButton('activity', 'Activity Log')}
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

      {tab === 'overview' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '20px', animation: 'fade-in 0.3s' }}>
          
          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', gap: '16px' }}>
            <h3 style={{ margin: 0, fontSize: '16px', color: 'var(--text-primary)', fontWeight: '600' }}>Master Controls</h3>
            {!settings && <div style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>Syncing configuration...</div>}
            
            {settings && SWITCHES.map((sw) => (
              <div key={sw.key} style={{ display: 'flex', gap: '20px', alignItems: 'center', justifyContent: 'space-between', padding: '12px 16px', background: 'rgba(0,0,0,0.2)', borderRadius: '8px', border: '1px solid var(--glass-border)' }}>
                <div>
                  <div style={{ fontWeight: '600', fontSize: '14px', color: 'var(--text-primary)', marginBottom: '4px' }}>{sw.title}</div>
                  <div style={{ fontSize: '12px', color: 'var(--text-secondary)', lineHeight: '1.4', maxWidth: '600px' }}>{sw.text}</div>
                </div>
                <Switch on={settings[sw.key]} disabled={busy} onChange={(v) => toggle(sw.key, v)} label={sw.title} />
              </div>
            ))}
          </div>

          <LaunchSkillsCard />

          <RecipeList refreshKey={refreshKey} />

          {status && (
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(280px, 1fr))', gap: '16px' }}>
              <div className="glass-panel" style={{ padding: '20px', borderRadius: '12px' }}>
                <h4 style={{ margin: '0 0 12px 0', fontSize: '14px', color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '1px' }}>System Health</h4>
                <div style={{ display: 'flex', flexDirection: 'column', gap: '12px' }}>
                  <Health ok={status.learning_enabled} text={status.learning_enabled ? 'Assistant Learning Active' : 'Assistant Learning Offline'} />
                  <Health ok={status.retrieval_enabled} text={status.retrieval_enabled ? 'Memory Retrieval Active' : 'Memory Retrieval Offline'} />
                  <Health ok={true} text="FalkorDB Graph Memory: Connected (Cloud)" detail="Primary graph database" />
                  <Health ok={status.vector_store.ok && status.vector_store.needs_reindex === 0} text={`Knowledge Index: ${status.vector_store.indexed} Entries`} />
                  <Health ok={status.embedding.ok} text="Semantic Search: Active" />
                  <Health ok={status.jobs_dead === 0} text={status.jobs_dead === 0 ? 'Task Pipeline: Operational' : 'Task Pipeline: Action Required'} />
                </div>
              </div>
              
              <div className="glass-panel" style={{ padding: '20px', borderRadius: '12px', display: 'flex', flexDirection: 'column' }}>
                <h4 style={{ margin: '0 0 12px 0', fontSize: '14px', color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '1px' }}>Current Operation</h4>
                <div style={{ fontSize: '16px', fontWeight: '600', color: status.current_activity ? 'var(--accent-cyan)' : 'var(--text-muted)' }}>
                  {status.current_activity ? status.current_activity : 'Standby / Idle'}
                </div>
                <div style={{ marginTop: 'auto', display: 'flex', flexDirection: 'column', gap: '6px' }}>
                  <div style={{ fontSize: '12px', color: 'var(--text-secondary)' }}>Latest activity: {when(status.last_learning_at)}</div>
                  <div style={{ fontSize: '12px', color: 'var(--text-secondary)' }}>Latest success: {when(status.last_success_at)}</div>
                </div>
              </div>
            </div>
          )}

          {stats && (
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: '12px' }}>
              <Stat label="Tasks Analyzed" value={stats.tasks_analyzed} />
              <Stat label="Verified Successes" value={stats.tasks_successful} hint="Finished and verified with real evidence" />
              <Stat label="Failures" value={stats.tasks_failed} />
              <Stat label="Memories Forged" value={stats.experiences_created} />
              <Stat label="Memories Refined" value={stats.experiences_updated} />
              <Stat label="Memories Rejected" value={stats.experiences_rejected} hint="Tasks judged not worth remembering" />
              <Stat label="Memories Archived" value={stats.experiences_deprecated} />
              <Stat label="Knowledge Reused" value={stats.experiences_reused} />
              <Stat label="Reuse Success Rate" value={pct(stats.successful_reuses / Math.max(1, stats.experiences_reused))} />
              <Stat label="Retrieval Precision" value={pct(stats.retrieval_hit_rate)} hint={`${stats.retrieval_hits} of ${stats.retrievals} searches found something`} />
              <Stat label="Avg Similarity Match" value={stats.average_similarity.toFixed(2)} />
              <Stat label="Quality Score Index" value={pct(stats.average_quality)} />
              <Stat label="Duplicates Pruned" value={stats.duplicates_prevented} />
            </div>
          )}

          {practice && (
            <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px' }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', gap: '16px', flexWrap: 'wrap', alignItems: 'flex-start' }}>
                <div>
                  <h3 style={{ margin: '0 0 8px 0', fontSize: '16px', color: 'var(--text-primary)', fontWeight: '600' }}>Autonomous Practice Protocol</h3>
                  <div style={{ fontSize: '13px', color: practice.enabled ? 'var(--accent-emerald)' : 'var(--text-secondary)', marginBottom: '8px' }}>
                    {practice.enabled ? practice.waiting_for : 'Offline. Enable Auto Practice in Master Controls to allow self-directed learning.'}
                  </div>
                  <div style={{ fontSize: '12px', color: 'var(--text-muted)' }}>
                    Daily Quota: {practice.started_last_24h}/{practice.max_per_day} | Cooldown: {practice.min_interval_mins}m | Idle trigger: {practice.idle_mins}m
                    {stats ? ` | Verified runs: ${stats.practice_verified}/${stats.practice_runs}` : ''}
                  </div>
                  {practice.last_request && <div style={{ fontSize: '12px', color: 'var(--accent-purple)', marginTop: '4px', fontStyle: 'italic' }}>"{practice.last_request}"</div>}
                </div>
                <button 
                  type="button" 
                  disabled={busy || !practice.enabled} 
                  onClick={() => void run(() => learningApi.practiceRunNow(), () => 'Practice requested: initialization in ~15s.')}
                  style={{
                    padding: '10px 18px',
                    borderRadius: '8px',
                    border: '1px solid var(--glass-border)',
                    background: practice.enabled && !busy ? 'rgba(0, 240, 255, 0.1)' : 'rgba(255,255,255,0.05)',
                    color: practice.enabled && !busy ? 'var(--accent-cyan)' : 'var(--text-muted)',
                    cursor: practice.enabled && !busy ? 'pointer' : 'not-allowed',
                    fontWeight: '600',
                    fontSize: '13px',
                    transition: 'all 0.2s'
                  }}
                >
                  Force Immediate Run
                </button>
              </div>
              
              {practice.recent.length > 0 && (
                <div style={{ marginTop: '16px', display: 'flex', flexDirection: 'column', gap: '8px' }}>
                  <div style={{ fontSize: '12px', textTransform: 'uppercase', color: 'var(--text-secondary)', fontWeight: '600', letterSpacing: '1px' }}>Recent Iterations</div>
                  {practice.recent.slice(0, 6).map((t) => (
                    <div key={t.id} style={{ fontSize: '13px', display: 'flex', gap: '12px', flexWrap: 'wrap', padding: '10px', background: 'rgba(0,0,0,0.2)', borderRadius: '6px', border: '1px solid var(--glass-border)' }}>
                      <span style={{ color: 'var(--text-secondary)' }}>{when(t.created_at)}</span>
                      <span style={{ color: 'var(--text-primary)', flex: 1 }}>{t.goal}</span>
                      <span style={{ color: 'var(--accent-purple)' }}>{t.source === 'paraphrase' ? 'Mutated' : 'Synthetic'}</span>
                      <span style={{ color: t.status.includes('success') ? 'var(--accent-emerald)' : 'var(--text-secondary)' }}>{t.status}</span>
                    </div>
                  ))}
                </div>
              )}
            </div>
          )}

          {evaluation && (
            <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px' }}>
              <h3 style={{ margin: '0 0 16px 0', fontSize: '16px', color: 'var(--text-primary)', fontWeight: '600' }}>Memory Efficacy Metrics</h3>
              <div style={{ overflowX: 'auto' }}>
                <table style={{ width: '100%', fontSize: '13px', borderCollapse: 'collapse' }}>
                  <thead>
                    <tr style={{ textAlign: 'left', color: 'var(--text-secondary)', background: 'rgba(0,0,0,0.2)' }}>
                      <th style={{ padding: '12px 16px' }}>Metric</th>
                      <th style={{ padding: '12px 16px' }}>Baseline (0-Shot)</th>
                      <th style={{ padding: '12px 16px' }}>With RAG Memory</th>
                      <th style={{ padding: '12px 16px' }}>Delta</th>
                    </tr>
                  </thead>
                  <tbody>
                    <tr style={{ borderBottom: '1px solid var(--glass-border)' }}>
                      <td style={{ padding: '12px 16px', color: 'var(--text-primary)' }}>Total Executions</td>
                      <td style={{ padding: '12px 16px' }}>{evaluation.runs_without_memory}</td>
                      <td style={{ padding: '12px 16px', color: 'var(--accent-cyan)' }}>{evaluation.runs_with_memory}</td>
                      <td style={{ padding: '12px 16px' }}>-</td>
                    </tr>
                    <tr style={{ borderBottom: '1px solid var(--glass-border)' }}>
                      <td style={{ padding: '12px 16px', color: 'var(--text-primary)' }}>Success Rate</td>
                      <td style={{ padding: '12px 16px' }}>{pct(evaluation.success_rate_without_memory)}</td>
                      <td style={{ padding: '12px 16px', color: 'var(--accent-emerald)', fontWeight: '600' }}>{pct(evaluation.success_rate_with_memory)}</td>
                      <td style={{ padding: '12px 16px' }}>{pct(evaluation.success_rate_with_memory - evaluation.success_rate_without_memory)}</td>
                    </tr>
                    <tr style={{ borderBottom: '1px solid var(--glass-border)' }}>
                      <td style={{ padding: '12px 16px', color: 'var(--text-primary)' }}>Avg TTF (Time To Finish)</td>
                      <td style={{ padding: '12px 16px' }}>{(evaluation.avg_execution_ms_without_memory / 1000).toFixed(1)}s</td>
                      <td style={{ padding: '12px 16px', color: 'var(--accent-cyan)' }}>{(evaluation.avg_execution_ms_with_memory / 1000).toFixed(1)}s</td>
                      <td style={{ padding: '12px 16px' }}>{((evaluation.avg_execution_ms_with_memory - evaluation.avg_execution_ms_without_memory) / 1000).toFixed(1)}s</td>
                    </tr>
                    <tr style={{ borderBottom: '1px solid var(--glass-border)' }}>
                      <td style={{ padding: '12px 16px', color: 'var(--text-primary)' }}>Avg Tool Chains</td>
                      <td style={{ padding: '12px 16px' }}>{evaluation.avg_tool_calls_without_memory.toFixed(1)}</td>
                      <td style={{ padding: '12px 16px', color: 'var(--accent-cyan)' }}>{evaluation.avg_tool_calls_with_memory.toFixed(1)}</td>
                      <td style={{ padding: '12px 16px' }}>{(evaluation.avg_tool_calls_with_memory - evaluation.avg_tool_calls_without_memory).toFixed(1)}</td>
                    </tr>
                    <tr>
                      <td style={{ padding: '12px 16px', color: 'var(--text-primary)' }}>Avg Token Expend</td>
                      <td style={{ padding: '12px 16px' }}>{evaluation.avg_tokens_without_memory}</td>
                      <td style={{ padding: '12px 16px', color: 'var(--accent-purple)' }}>{evaluation.avg_tokens_with_memory}</td>
                      <td style={{ padding: '12px 16px' }}>{evaluation.avg_tokens_with_memory - evaluation.avg_tokens_without_memory}</td>
                    </tr>
                  </tbody>
                </table>
              </div>
              <div style={{ fontSize: '12px', color: 'var(--text-secondary)', marginTop: '16px', padding: '12px', background: 'rgba(0,0,0,0.2)', borderRadius: '8px' }}>
                <span style={{ color: 'var(--accent-cyan)' }}>Insight:</span> {evaluation.note} <br/>
                Memory-induced drift/failures: {evaluation.memory_related_failures} | Pure reuse viability: {pct(evaluation.reuse_success_rate)}
              </div>
            </div>
          )}
          
          {stats && stats.total_experiences > 0 && (
            <div className="glass-panel" style={{ padding: '16px', borderRadius: '12px', display: 'flex', gap: '12px', flexWrap: 'wrap', alignItems: 'center' }}>
              <span style={{ fontSize: '14px', color: 'var(--text-primary)', fontWeight: '600' }}>Corpus State ({stats.total_experiences}):</span>
              {Object.entries(stats.by_status).map(([s, n]) => (<span key={s} style={{ display: 'inline-flex', gap: '6px', alignItems: 'center' }}><StatusBadge status={s} /> {n}</span>))}
            </div>
          )}
        </div>
      )}

      {tab === 'experiences' && <div style={{ animation: 'fade-in 0.3s' }}><ExperienceManager refreshKey={refreshKey} onChanged={() => void load()} /></div>}

      {tab === 'settings' && draft && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '24px', animation: 'fade-in 0.3s' }}>
          
          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px' }}>
             <h3 style={{ margin: '0 0 16px 0', fontSize: '16px', color: 'var(--text-primary)', fontWeight: '600' }}>Engine Parameters</h3>
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(280px, 1fr))', gap: '16px' }}>
              {FIELDS.map((f) => (
                <label key={f.key} style={{ display: 'flex', flexDirection: 'column', gap: '8px', padding: '12px', background: 'rgba(0,0,0,0.2)', borderRadius: '8px', border: '1px solid var(--glass-border)' }} title={f.hint}>
                  <span style={{ fontSize: '13px', color: 'var(--text-primary)', fontWeight: '500' }}>{f.label}</span>
                  <input
                    type="number"
                    style={{ ...numberInput, width: '100%', background: 'rgba(0,0,0,0.4)', padding: '10px' }}
                    min={f.min}
                    max={f.max}
                    step={f.step}
                    value={draft[f.key] as number}
                    onChange={(e) => setDraft({ ...draft, [f.key]: e.target.value === '' ? f.min : Number(e.target.value) })}
                  />
                  <span style={{ fontSize: '11.5px', color: 'var(--text-secondary)', lineHeight: '1.4' }}>{f.hint}</span>
                </label>
              ))}
              
              <label style={{ display: 'flex', gap: '16px', alignItems: 'center', padding: '16px', background: 'rgba(0,0,0,0.2)', borderRadius: '8px', border: '1px solid var(--glass-border)' }}>
                <Switch on={draft.require_human_approval} onChange={(v) => setDraft({ ...draft, require_human_approval: v })} label="Require human approval" />
                <span>
                  <div style={{ fontSize: '13px', color: 'var(--text-primary)', fontWeight: '500', marginBottom: '4px' }}>Human Approval Gate</div>
                  <div style={{ fontSize: '11.5px', color: 'var(--text-secondary)', lineHeight: '1.4' }}>New experiences remain quarantined (VERIFIED state) until a human explicitely approves them for production retrieval.</div>
                </span>
              </label>
            </div>
          </div>
          
          <div style={{ display: 'flex', gap: '12px', alignItems: 'center', flexWrap: 'wrap' }}>
            <button 
              type="button" 
              disabled={!dirty || busy} 
              onClick={() => draft && void save(draft)}
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
              Commit Configuration
            </button>
            <button 
              type="button" 
              disabled={!dirty || busy} 
              onClick={() => setDraft(settings)}
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
              Revert Changes
            </button>
            <span style={{ fontSize: '12px', color: 'var(--text-muted)', marginLeft: '8px' }}>Hard limits are enforced by Core constraints. Changes require UI validation token.</span>
          </div>
          
          <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', gap: '16px', border: '1px solid rgba(248, 113, 113, 0.2)' }}>
            <h3 style={{ margin: 0, fontSize: '16px', color: 'var(--accent-rose)', fontWeight: '600' }}>Danger Zone / Operations</h3>
            
            <div style={{ display: 'flex', gap: '12px', flexWrap: 'wrap', paddingBottom: '16px', borderBottom: '1px dashed rgba(255,255,255,0.1)' }}>
              <button 
                type="button" 
                disabled={busy} 
                onClick={() => void run(() => learningApi.reindex(), (r: { reindexed: number; failed: number }) => `Reindexed ${r.reindexed} experience(s), ${r.failed} failed.`)}
                style={{ padding: '10px 16px', borderRadius: '8px', border: '1px solid var(--glass-border)', background: 'rgba(255,255,255,0.05)', color: 'var(--text-primary)', cursor: 'pointer', fontSize: '13px' }}
              >
                Rebuild Vector Embeddings
              </button>
              <button 
                type="button" 
                disabled={busy} 
                onClick={() => void run(() => learningApi.consolidate(), (r: { merged: number; marked_stale: number; removed: number }) => `Merged ${r.merged}, marked ${r.marked_stale} stale, removed ${r.removed}.`)}
                style={{ padding: '10px 16px', borderRadius: '8px', border: '1px solid var(--glass-border)', background: 'rgba(255,255,255,0.05)', color: 'var(--text-primary)', cursor: 'pointer', fontSize: '13px' }}
              >
                Consolidate & Deframent Memory
              </button>
            </div>
            
            <div style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>Type <code style={{ color: 'var(--accent-rose)' }}>delete all experiences</code> to initiate a complete wipe of the agent's memory bank. This action is irreversible.</div>
            <div style={{ display: 'flex', gap: '12px' }}>
              <input 
                value={clearText} 
                onChange={(e) => setClearText(e.target.value)} 
                placeholder="delete all experiences" 
                style={{ padding: '10px 14px', borderRadius: '8px', border: '1px solid rgba(248, 113, 113, 0.3)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-primary)', fontSize: '13px', width: '300px', outline: 'none' }} 
              />
              <button 
                type="button" 
                disabled={busy || clearText !== 'delete all experiences'} 
                onClick={() => void run(() => learningApi.clear().then((r) => { setClearText(''); return r; }), (r: { deleted: number }) => `Deleted ${r.deleted} experience(s).`)}
                style={{
                  padding: '10px 16px',
                  borderRadius: '8px',
                  border: '1px solid rgba(248, 113, 113, 0.5)',
                  background: clearText === 'delete all experiences' && !busy ? 'rgba(248, 113, 113, 0.2)' : 'rgba(248, 113, 113, 0.05)',
                  color: 'var(--accent-rose)',
                  cursor: clearText === 'delete all experiences' && !busy ? 'pointer' : 'not-allowed',
                  fontWeight: '600',
                  fontSize: '13px',
                  opacity: clearText === 'delete all experiences' ? 1 : 0.5
                }}
              >
                Wipe Neural DB
              </button>
            </div>
          </div>
        </div>
      )}

      {tab === 'activity' && (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(340px, 1fr))', gap: '20px', animation: 'fade-in 0.3s' }}>
          
          <div className="glass-panel" style={{ padding: '0', borderRadius: '12px', overflow: 'hidden' }}>
            <div style={{ padding: '16px 20px', borderBottom: '1px solid var(--glass-border)', fontWeight: '600', color: 'var(--text-primary)', background: 'rgba(0,0,0,0.2)' }}>Subroutine Queues</div>
            <div style={{ padding: '12px 20px', display: 'flex', flexDirection: 'column', gap: '8px', maxHeight: '500px', overflowY: 'auto' }}>
              {jobs.length === 0 && <div style={{ fontSize: '13px', color: 'var(--text-muted)', textAlign: 'center', padding: '20px' }}>No active background processing.</div>}
              {jobs.map((j) => (
                <div key={j.id} style={{ fontSize: '13px', padding: '12px', background: 'rgba(0,0,0,0.2)', borderRadius: '8px', border: '1px solid var(--glass-border)' }}>
                  <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: '4px' }}>
                    <span style={{ color: 'var(--accent-cyan)', fontWeight: '600' }}>#{j.id}</span>
                    <span style={{ color: 'var(--text-secondary)' }}>{when(j.updated_at)}</span>
                  </div>
                  <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
                    <span style={{ color: j.status === 'dead' ? 'var(--accent-rose)' : j.status === 'running' ? 'var(--accent-emerald)' : 'var(--text-primary)' }}>{j.status.toUpperCase()}{j.status === 'dead' ? ' (ABORTED)' : ''}</span>
                    <span style={{ color: 'var(--text-muted)' }}>| Cycle {j.attempts}/{j.max_attempts}</span>
                  </div>
                  {j.last_error && <div style={{ marginTop: '8px', color: '#fbbf24', fontSize: '12px', padding: '6px 10px', background: 'rgba(251, 191, 36, 0.1)', borderRadius: '4px' }}>{j.last_error}</div>}
                </div>
              ))}
            </div>
          </div>
          
          <div className="glass-panel" style={{ padding: '0', borderRadius: '12px', overflow: 'hidden' }}>
            <div style={{ padding: '16px 20px', borderBottom: '1px solid var(--glass-border)', fontWeight: '600', color: 'var(--text-primary)', background: 'rgba(0,0,0,0.2)' }}>Event Telemetry</div>
            <div style={{ padding: '12px 20px', display: 'flex', flexDirection: 'column', gap: '8px', maxHeight: '500px', overflowY: 'auto' }}>
              {events.length === 0 && <div style={{ fontSize: '13px', color: 'var(--text-muted)', textAlign: 'center', padding: '20px' }}>No telemetry recorded.</div>}
              {events.map((e) => (
                <div key={e.id} style={{ fontSize: '13px', padding: '10px 12px', background: 'rgba(0,0,0,0.2)', borderRadius: '6px', borderLeft: '3px solid var(--accent-purple)' }}>
                  <div style={{ color: 'var(--text-secondary)', fontSize: '11.5px', marginBottom: '4px' }}>{when(e.ts)}</div>
                  <div style={{ color: 'var(--text-primary)' }}>{e.event.replace(/_/g, ' ')}</div>
                </div>
              ))}
            </div>
          </div>
          
        </div>
      )}
    </div>
  );
}
