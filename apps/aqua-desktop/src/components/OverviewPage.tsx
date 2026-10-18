import {
  CheckCircle2,
  ChevronRight,
  FolderSearch,
  Headphones,
  Lock,
  ShieldAlert,
  Sparkles,
  Volume2,
  Workflow,
  Zap,
  Network,
} from 'lucide-react';
import type { CoreStatusResponse } from '../api/coreStatus';
import type { TabId } from '../nav';
import { PageHeader, Pill, Section } from './ui';

interface Props {
  status: CoreStatusResponse | null;
  loading: boolean;
  connected: boolean;
  /** Starting text while assistant initializes */
  starting?: string;
  onOpen: (tab: TabId) => void;
}

export function OverviewPage({ status, loading, connected, starting, onOpen }: Props) {
  let headline: string;
  if (!connected && starting) headline = starting;
  else if (!connected) headline = loading ? 'Connecting to assistant...' : 'Assistant is currently offline.';
  else if (status?.pipeline_ready) headline = 'Assistant is active and ready. Say "wake up" or click the desktop orb to begin.';
  else headline = 'Assistant services are ready and active.';

  const isVoiceSecurityConfigured = status?.voice_security?.enabled && status?.biometric_enrolled;

  const quickLinks = [
    {
      tab: 'tts-voices' as TabId,
      icon: Volume2,
      title: 'Voice Profiles',
      note: 'Choose natural speech voices and audio styles.',
    },
    {
      tab: 'flows' as TabId,
      icon: Workflow,
      title: 'Automated Routines',
      note: 'Set up automated workflows and daily shortcuts.',
    },
    {
      tab: 'graph-memory' as TabId,
      icon: Network,
      title: 'Memory Graph (FalkorDB)',
      note: 'Associative memory and skill coordination graph.',
    },
    {
      tab: 'tools' as TabId,
      icon: ShieldAlert,
      title: 'App Approvals',
      note: 'Control application permissions and approval rules.',
    },
    {
      tab: 'security' as TabId,
      icon: Lock,
      title: 'Voice Security',
      note: 'Manage your voice profile and unlock security.',
    },
    {
      tab: 'vault' as TabId,
      icon: FolderSearch,
      title: 'Secure Vault',
      note: 'Manage private files and personal document index.',
    },
    {
      tab: 'devices' as TabId,
      icon: Headphones,
      title: 'Audio Devices',
      note: 'Configure microphone input and speaker output.',
    },
  ];

  return (
    <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 24 }}>
      <PageHeader
        title="Overview"
        subtitle={headline}
      />

      {/* Hero Welcome Card */}
      <div
        className="glass-panel"
        style={{
          padding: '28px 32px',
          borderRadius: 16,
          background: 'linear-gradient(135deg, rgba(6, 182, 212, 0.08) 0%, rgba(139, 92, 246, 0.08) 100%)',
          border: '1px solid rgba(255, 255, 255, 0.08)',
          display: 'flex',
          flexDirection: 'column',
          gap: 18,
          position: 'relative',
          overflow: 'hidden',
        }}
      >
        <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', flexWrap: 'wrap', gap: 16 }}>
          <div style={{ maxWidth: 620, display: 'flex', flexDirection: 'column', gap: 8 }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
              <Sparkles size={18} color="var(--accent-cyan)" />
              <span style={{ fontSize: 13, fontWeight: 600, color: 'var(--accent-cyan)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
                Private Desktop Assistant
              </span>
            </div>
            <h2 style={{ fontSize: 22, fontWeight: 700, margin: 0, color: 'var(--text-primary)' }}>
              Ready for your commands
            </h2>
            <p style={{ fontSize: 14, color: 'var(--text-secondary)', lineHeight: 1.6, margin: 0 }}>
              Aqua operates locally on your machine with total privacy. All voice detection, interactions, and file queries remain strictly on your device.
            </p>
          </div>

          <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
            <Pill tone={connected ? 'ok' : 'attention'} label="System" value={connected ? 'Active' : 'Connecting'} />
            <Pill tone="ok" label="Privacy" value="100% On-Device" />
          </div>
        </div>

        {/* Feature Highlights */}
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))', gap: 12, marginTop: 4 }}>
          <div style={{ padding: '14px 16px', borderRadius: 10, background: 'rgba(0,0,0,0.25)', border: '1px solid var(--glass-border)', display: 'flex', alignItems: 'center', gap: 12 }}>
            <CheckCircle2 size={18} color="var(--accent-emerald)" />
            <div>
              <div style={{ fontSize: 13, fontWeight: 600 }}>Voice Interaction</div>
              <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>Hands-free speech active</div>
            </div>
          </div>
          <div style={{ padding: '14px 16px', borderRadius: 10, background: 'rgba(0,0,0,0.25)', border: '1px solid var(--glass-border)', display: 'flex', alignItems: 'center', gap: 12 }}>
            <Zap size={18} color="var(--accent-cyan)" />
            <div>
              <div style={{ fontSize: 13, fontWeight: 600 }}>Smart Automations</div>
              <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>Routines & tasks ready</div>
            </div>
          </div>
          <div style={{ padding: '14px 16px', borderRadius: 10, background: 'rgba(0,0,0,0.25)', border: '1px solid var(--glass-border)', display: 'flex', alignItems: 'center', gap: 12 }}>
            <Lock size={18} color={isVoiceSecurityConfigured ? 'var(--accent-purple)' : 'var(--text-muted)'} />
            <div>
              <div style={{ fontSize: 13, fontWeight: 600 }}>Voice Security</div>
              <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>{isVoiceSecurityConfigured ? 'Protected' : 'Standard Access'}</div>
            </div>
          </div>
        </div>
      </div>

      {/* WeMakeDevs FalkorDB Hackathon Spotlight Card */}
      <div
        className="glass-panel"
        style={{
          padding: '22px 26px',
          borderRadius: 16,
          background: 'linear-gradient(135deg, rgba(60, 203, 255, 0.1) 0%, rgba(176, 106, 255, 0.1) 100%)',
          border: '1px solid rgba(60, 203, 255, 0.25)',
          display: 'flex',
          flexDirection: 'column',
          gap: 14,
        }}
      >
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: 10 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <Sparkles size={18} color="var(--accent-cyan)" />
            <h3 style={{ margin: 0, fontSize: 16, fontWeight: 700, color: '#fff' }}>
              WeMakeDevs Hackathon: Graph Hacks Powered by FalkorDB
            </h3>
          </div>
          <span style={{ fontSize: 11, fontWeight: 700, color: 'var(--accent-cyan)', background: 'rgba(60,203,255,0.15)', padding: '3px 10px', borderRadius: 20 }}>
            3-Track Showcase
          </span>
        </div>

        <p style={{ margin: 0, fontSize: 13.5, color: 'var(--text-secondary)', lineHeight: 1.6 }}>
          Project Aqua integrates FalkorDB as its primary context and associative memory engine across all three hackathon tracks:
          multi-hop incident reasoning, cross-session memory recall, and unified organizational knowledge.
        </p>

        <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', marginTop: 4 }}>
          <button
            className="btn-primary"
            onClick={() => onOpen('company-brain')}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 8,
              padding: '9px 18px',
              fontSize: 13,
              fontWeight: 700,
              background: 'linear-gradient(135deg, var(--accent-cyan), var(--accent-blue))',
              color: '#020617',
              borderRadius: 8,
              border: 'none',
              cursor: 'pointer',
            }}
          >
            Launch Company Brain (Tracks 1 & 3) <ChevronRight size={15} />
          </button>
          <button
            className="btn-ghost"
            onClick={() => onOpen('graph-memory')}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 8,
              padding: '9px 18px',
              fontSize: 13,
              fontWeight: 600,
              borderRadius: 8,
              border: '1px solid var(--glass-border-highlight)',
              color: 'var(--text-primary)',
              cursor: 'pointer',
            }}
          >
            Explore Memory Graph (Track 2) <ChevronRight size={15} />
          </button>
        </div>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(320px, 1fr))', gap: 20 }}>
        {/* System Status Section */}
        <Section title="System Status" hint="Operational status of your assistant services.">
          <dl style={{ display: 'grid', gridTemplateColumns: 'auto 1fr', gap: '12px 18px', fontSize: 14, alignItems: 'center', margin: 0 }}>
            <dt style={{ color: 'var(--text-secondary)' }}>Connection</dt>
            <dd style={{ margin: 0 }}>
              <Pill
                tone={connected ? 'ok' : starting ? 'attention' : 'problem'}
                label={connected ? 'Online (Local)' : starting ? 'Starting' : 'Disconnected'}
              />
            </dd>
            <dt style={{ color: 'var(--text-secondary)' }}>Interaction</dt>
            <dd style={{ margin: 0 }}>
              <Pill
                tone={status?.pipeline_ready ? 'ok' : status?.text_pipeline_ready ? 'attention' : 'problem'}
                label={status?.pipeline_ready ? 'Voice & Text Active' : status?.text_pipeline_ready ? 'Text Only' : 'Initializing'}
              />
            </dd>
            <dt style={{ color: 'var(--text-secondary)' }}>Local Storage</dt>
            <dd style={{ margin: 0 }}>
              <Pill
                tone={status?.database_ready ? 'ok' : 'attention'}
                label={status?.database_ready ? 'Ready' : 'Initializing'}
              />
            </dd>
            <dt style={{ color: 'var(--text-secondary)' }}>Voice Security</dt>
            <dd style={{ margin: 0 }}>
              <Pill
                tone={isVoiceSecurityConfigured ? 'ok' : 'neutral'}
                label={isVoiceSecurityConfigured ? 'Enrolled & Protected' : 'Setup Available'}
              />
            </dd>
          </dl>
        </Section>

        {/* Quick Access Section */}
        <Section title="Quick Access" hint="Frequently accessed assistant controls.">
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 8 }}>
            {quickLinks.map((l) => (
              <button
                key={l.tab}
                className="btn-ghost"
                onClick={() => onOpen(l.tab)}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 12,
                  textAlign: 'left',
                  padding: '12px 14px',
                  width: '100%',
                  borderRadius: 10,
                  border: '1px solid var(--glass-border)',
                  background: 'rgba(255, 255, 255, 0.02)',
                  cursor: 'pointer',
                  transition: 'background 0.2s, transform 0.15s',
                }}
              >
                <div style={{ width: 34, height: 34, borderRadius: 8, background: 'rgba(6, 182, 212, 0.1)', display: 'grid', placeItems: 'center', flexShrink: 0 }}>
                  <l.icon size={17} color="var(--accent-cyan)" />
                </div>
                <span style={{ flex: 1, display: 'flex', flexDirection: 'column', gap: 2 }}>
                  <span style={{ color: 'var(--text-primary)', fontWeight: 600, fontSize: 13.5 }}>{l.title}</span>
                  <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>{l.note}</span>
                </span>
                <ChevronRight size={15} color="var(--text-muted)" />
              </button>
            ))}
          </div>
        </Section>
      </div>
    </div>
  );
}
