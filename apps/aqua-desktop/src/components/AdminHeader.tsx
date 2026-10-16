import { ShieldCheck } from 'lucide-react';
import { Pill } from './ui';

interface AdminHeaderProps {
  voiceStandbyActive: boolean;
  biometricStatus: string;
  coreConnected?: boolean;
  coreStarting?: string;
}

export function AdminHeader({ voiceStandbyActive, biometricStatus, coreConnected = false, coreStarting }: AdminHeaderProps) {
  const displayStatus = biometricStatus === 'Master enrolled' ? 'Protected' : biometricStatus === 'Not enrolled' ? 'Setup Available' : biometricStatus;

  return (
    <header className="shell-header">
      <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
        <div style={{ width: 34, height: 34, borderRadius: 10, background: 'linear-gradient(135deg, var(--accent-cyan), var(--accent-blue))', display: 'grid', placeItems: 'center', fontFamily: 'var(--font-display)', fontWeight: 700, fontSize: 17, color: '#04121c' }}>
          A
        </div>
        <div>
          <h1 style={{ fontSize: 17, fontWeight: 650, lineHeight: 1.2 }}>Aqua</h1>
          <p style={{ fontSize: 12, color: 'var(--text-muted)' }}>Admin console</p>
        </div>
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
        <Pill tone={coreConnected ? 'ok' : 'attention'} live={coreConnected} label="Assistant" value={coreConnected ? 'Online & Ready' : coreStarting ?? 'Connecting...'} />
        <Pill tone={voiceStandbyActive ? 'ok' : 'neutral'} label="Voice Assistant" value={voiceStandbyActive ? 'Active' : 'Standby'} title="Voice detection is ready for hands-free interactions." />
        <Pill tone="ok" label="Privacy" value="Local & Private" title="All processing occurs securely on your local device." />
        <span className="pill" title="Voice security verification status">
          <ShieldCheck size={14} color="var(--accent-purple)" />
          <span>Voice Security</span>
          <strong>{displayStatus}</strong>
        </span>
      </div>
    </header>
  );
}
