import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { KeyboardEvent } from 'react';
import { useCoreStatus } from './hooks/useCoreStatus';
import { ALL_TABS, NAV_GROUPS, TabId } from './nav';
import { AdminHeader } from './components/AdminHeader';
import { TaskBar } from './components/TaskBar';
import { OverviewPage } from './components/OverviewPage';
import { VoiceEnrollmentWizard } from './components/VoiceEnrollmentWizard';
import { FileIndexPanel } from './components/FileIndexPanel';
import { DevicesPanel } from './components/DevicesPanel';
import { OrbPositionPanel } from './components/OrbPositionPanel';
import { OnboardingWizard } from './components/OnboardingWizard';
import { TtsVoiceSelectorPanel } from './components/TtsVoiceSelectorPanel';
import { BrowserConfirmationPrompt } from './components/BrowserConfirmationPrompt';
import { ToolsPanel } from './components/ToolsPanel';
import { AgentLearningPanel } from './components/AgentLearningPanel';
import { WorkflowsPanel } from './components/WorkflowsPanel';
import { FlowsPanel } from './components/FlowsPanel';
import { PermissionsPage } from './components/PermissionsPage';
import { ReminderToast } from './components/ReminderToast';
import { MemoryPage } from './components/MemoryPage';
import { VaultPage } from './components/VaultPage';
import { SkillsInbox } from './components/SkillsInbox';
import { GraphMemoryExplorer } from './components/GraphMemoryExplorer';
import { CompanyBrainPage } from './components/CompanyBrainPage';
import { ProductShowcasePage } from './components/ProductShowcasePage';

const TAB_KEY = 'aquaActiveTab';
const CORE_DEPENDENT_TABS = new Set<TabId>([
  'tts-voices', 'security', 'workflows', 'flows', 'tools', 'permissions', 'memory',
  'agent-learning', 'skills-inbox', 'file-index', 'vault', 'devices',
]);

function initialTab(): TabId {
  const params = new URLSearchParams(window.location.search);
  if (params.has('demo')) return 'company-brain';
  const showcaseTab = params.get('showcase') as TabId | null;
  if (showcaseTab === 'showcase-capabilities' || showcaseTab === 'showcase-platform') return showcaseTab;
  if (showcaseTab && ALL_TABS.includes(showcaseTab)) return showcaseTab;
  try {
    const saved = localStorage.getItem(TAB_KEY) as TabId | null;
    if (saved && ALL_TABS.includes(saved)) return saved;
  } catch { /* the console still works without storage */ }
  return 'overview';
}

export default function App() {
  const [activeTab, setActiveTabState] = useState<TabId>(initialTab);
  const [setupComplete, setSetupCompleteState] = useState(() => {
    const params = new URLSearchParams(window.location.search);
    if (params.has('demo') || params.has('showcase')) return true;
    try { return localStorage.getItem('aquaSetupComplete') === 'true'; } catch { return false; }
  });

  const setActiveTab = useCallback((tab: TabId) => {
    setActiveTabState(tab);
    try { localStorage.setItem(TAB_KEY, tab); } catch { /* not remembered, that is all */ }
  }, []);

  // Stable, so the wizard's "required models are present" effect runs once.
  const markSetupComplete = useCallback(() => {
    try { localStorage.setItem('aquaSetupComplete', 'true'); } catch { /* the page still works without storage */ }
    setSetupCompleteState(true);
  }, []);

  // While this window is not the one in use (another window has focus, it is minimised or hidden), the decorative
  // idle animations are paused. Animating a window costs the processor even when nobody is looking at it, and the
  // language model is slower for it. See `html[data-inactive]` in index.css.
  useEffect(() => {
    const root = document.documentElement;
    const sync = () => root.toggleAttribute('data-inactive', document.hidden || !document.hasFocus());
    sync();
    window.addEventListener('focus', sync);
    window.addEventListener('blur', sync);
    document.addEventListener('visibilitychange', sync);
    return () => {
      window.removeEventListener('focus', sync);
      window.removeEventListener('blur', sync);
      document.removeEventListener('visibilitychange', sync);
      root.removeAttribute('data-inactive');
    };
  }, []);

  const { status, loading, connected, starting } = useCoreStatus();

  const voiceStandbyActive = status?.voice_standby_active ?? false;
  const biometricStatus = loading ? 'Checking…' : status?.biometric_enrolled ? 'Master enrolled' : 'Not enrolled';

  // The marker behind the active menu item slides to it.
  const navRef = useRef<HTMLElement>(null);
  const itemRefs = useRef<Partial<Record<TabId, HTMLButtonElement | null>>>({});
  const indicatorRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = itemRefs.current[activeTab];
    const bar = indicatorRef.current;
    if (!el || !bar) return;
    bar.style.transform = `translateY(${el.offsetTop}px)`;
    bar.style.height = `${el.offsetHeight}px`;
    bar.style.opacity = '1';
  }, [activeTab]);

  const onNavKey = (e: KeyboardEvent) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    const i = ALL_TABS.indexOf(activeTab);
    const next = ALL_TABS[Math.max(0, Math.min(ALL_TABS.length - 1, i + (e.key === 'ArrowDown' ? 1 : -1)))];
    if (next === activeTab) return;
    e.preventDefault();
    setActiveTab(next);
    itemRefs.current[next]?.focus();
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100vh', backgroundColor: 'var(--bg-primary)' }}>
      {!setupComplete && (
        <OnboardingWizard
          connected={connected}
          services={status ? {
            llm_gguf: status.models.llm_gguf,
            whisper_ggml: status.models.whisper_ggml,
            kokoro_onnx: status.models.kokoro_onnx,
            deepfilternet_onnx: status.models.deepfilternet_onnx,
            biometrics_onnx: status.models.biometrics_onnx,
          } : null}
          onComplete={markSetupComplete}
        />
      )}
      {/* Browser approvals (unknown sites, Send/Pay/Delete, passwords) can come from any task */}
      <BrowserConfirmationPrompt />
      {/* Reminders and timers Aqua says on its own: the player and its one toast at a time */}
      <ReminderToast />

      <AdminHeader
        voiceStandbyActive={voiceStandbyActive}
        biometricStatus={biometricStatus}
        coreConnected={connected}
        coreStarting={starting?.short}
      />

      <TaskBar />

      <div style={{ display: 'flex', flex: 1, overflow: 'hidden', minHeight: 0 }}>
        <nav className="nav" ref={navRef} aria-label="Admin pages" onKeyDown={onNavKey}>
          <div className="nav-indicator" ref={indicatorRef} aria-hidden />
          {NAV_GROUPS.map((group) => (
            <div key={group.label} style={{ display: 'contents' }}>
              <div className="nav-group-label">{group.label}</div>
              {group.items.map((item) => {
                const active = activeTab === item.id;
                return (
                  <button
                    key={item.id}
                    ref={(el) => { itemRefs.current[item.id] = el; }}
                    className="nav-item"
                    aria-current={active ? 'page' : undefined}
                    onClick={() => setActiveTab(item.id)}
                  >
                    <item.icon size={17} color={active ? 'var(--accent-cyan)' : 'currentColor'} />
                    <span className="nav-label">{item.label}</span>
                  </button>
                );
              })}
            </div>
          ))}
        </nav>

        {/* Remounting on the key plays the short entrance, so choosing a page is acknowledged */}
        <main style={{ flex: 1, minWidth: 0, padding: 24, overflowY: 'auto' }}>
          {!loading && !connected && CORE_DEPENDENT_TABS.has(activeTab) && (
            <div
              role="status"
              className="glass-panel page-enter"
              style={{
                maxWidth: 1200,
                margin: '0 auto 18px',
                padding: '12px 16px',
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'space-between',
                gap: 14,
                flexWrap: 'wrap',
                borderColor: 'rgba(255, 176, 60, 0.25)',
                background: 'linear-gradient(90deg, rgba(255, 176, 60, 0.08), rgba(60, 203, 255, 0.04))',
              }}
            >
              <div style={{ display: 'flex', alignItems: 'center', gap: 11 }}>
                <span className="pulse-dot" style={{ width: 9, height: 9, borderRadius: '50%', background: 'var(--state-attention)', color: 'var(--state-attention)', flexShrink: 0 }} />
                <div>
                  <strong style={{ display: 'block', fontSize: 13.5 }}>Aqua Core is connecting</strong>
                  <span style={{ color: 'var(--text-secondary)', fontSize: 12.5 }}>
                    This page is ready; live data and controls appear automatically when the private local service responds.
                  </span>
                </div>
              </div>
              <span style={{ color: 'var(--text-muted)', fontSize: 11.5, fontFamily: 'var(--font-mono)' }}>LOCAL · PRIVATE · AUTO-RETRY</span>
            </div>
          )}
          <div key={activeTab} className="page-enter">
            {activeTab === 'overview' && (
              <OverviewPage status={status} loading={loading} connected={connected} starting={starting?.text} onOpen={setActiveTab} />
            )}
            {activeTab === 'tts-voices' && <TtsVoiceSelectorPanel />}
            {activeTab === 'security' && <VoiceEnrollmentWizard />}
            {activeTab === 'workflows' && <WorkflowsPanel />}
            {activeTab === 'flows' && <FlowsPanel />}
            {activeTab === 'tools' && <ToolsPanel />}
            {activeTab === 'permissions' && <PermissionsPage />}
            {activeTab === 'memory' && <MemoryPage />}
            {activeTab === 'graph-memory' && <GraphMemoryExplorer />}
            {activeTab === 'company-brain' && <CompanyBrainPage />}
            {activeTab === 'agent-learning' && <AgentLearningPanel />}
            {activeTab === 'skills-inbox' && <SkillsInbox />}
            {activeTab === 'file-index' && <FileIndexPanel />}
            {activeTab === 'vault' && <VaultPage />}
            {activeTab === 'devices' && <DevicesPanel />}
            {activeTab === 'orb-position' && <OrbPositionPanel />}
            {activeTab === 'showcase-capabilities' && <ProductShowcasePage variant="capabilities" />}
            {activeTab === 'showcase-platform' && <ProductShowcasePage variant="platform" />}
          </div>
        </main>
      </div>
    </div>
  );
}
