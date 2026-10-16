import {
  Bot, Brain, Database, FileSearch, GitBranch, Headphones, Laptop,
  LockKeyhole, Mic2, Network, ShieldCheck, Sparkles, Vault, Workflow,
} from 'lucide-react';

const groups = [
  {
    title: 'Voice & interaction',
    accent: '#38bdf8',
    items: [
      [Mic2, 'Voice profiles', 'Local speech voices and preview controls'],
      [LockKeyhole, 'Voice security', 'Enrollment and biometric verification'],
      [Headphones, 'Audio devices', 'Microphone and speaker configuration'],
    ],
  },
  {
    title: 'Automation & control',
    accent: '#a78bfa',
    items: [
      [Workflow, 'Workflows', 'Repeatable multi-step task execution'],
      [GitBranch, 'Automated routines', 'Scheduled actions and shortcuts'],
      [ShieldCheck, 'Approvals & privacy', 'Policy, consent, and audit controls'],
    ],
  },
  {
    title: 'Knowledge & learning',
    accent: '#34d399',
    items: [
      [Network, 'Company Brain', 'Graph-grounded answers and actions'],
      [Brain, 'Memory & learning', 'Inspectable recall, preferences, and corrections'],
      [FileSearch, 'File search & skills', 'Local indexing and reviewed extensions'],
      [Vault, 'Secure Vault', 'Protected personal document context'],
    ],
  },
];

export function ProductShowcasePage({ variant }: { variant: 'capabilities' | 'platform' }) {
  if (variant === 'platform') {
    return (
      <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 24 }}>
        <div className="glass-panel" style={{ padding: 34, borderRadius: 18, border: '1px solid rgba(56,189,248,.28)', background: 'linear-gradient(135deg, rgba(14,165,233,.10), rgba(139,92,246,.08))' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 14 }}>
            <Laptop size={34} color="#38bdf8" />
            <div>
              <div style={{ color: '#38bdf8', fontWeight: 700, letterSpacing: '.08em', textTransform: 'uppercase', fontSize: 13 }}>Platform support</div>
              <h1 style={{ margin: '6px 0 0', fontSize: 34 }}>Aqua for Windows desktop</h1>
            </div>
          </div>
          <p style={{ color: 'var(--text-secondary)', fontSize: 17, lineHeight: 1.65, maxWidth: 900, margin: '22px 0 0' }}>
            The current demo and verified application target Windows. Aqua combines an installable desktop shell, local AI services, privacy controls, and an optional FalkorDB knowledge layer.
          </p>
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 18 }}>
          {[
            [Laptop, 'Windows', 'Current validated desktop target', 'SUPPORTED'],
            [Database, 'FalkorDB', 'Local Docker or configured cloud endpoint', 'SUPPORTED'],
            [Bot, 'Local-first AI', 'Voice, memory, files, and policy services', 'ARCHITECTURE'],
          ].map(([Icon, title, text, badge]) => {
            const CardIcon = Icon as typeof Laptop;
            return (
              <div key={title as string} className="glass-panel" style={{ padding: 24, borderRadius: 14, minHeight: 180 }}>
                <CardIcon size={27} color="#38bdf8" />
                <div style={{ color: '#34d399', fontSize: 11, fontWeight: 800, letterSpacing: '.08em', marginTop: 20 }}>{badge as string}</div>
                <h2 style={{ margin: '7px 0', fontSize: 22 }}>{title as string}</h2>
                <p style={{ color: 'var(--text-secondary)', lineHeight: 1.55, margin: 0 }}>{text as string}</p>
              </div>
            );
          })}
        </div>

        <div className="glass-panel" style={{ padding: '20px 24px', borderRadius: 14, display: 'flex', alignItems: 'center', gap: 14 }}>
          <ShieldCheck size={24} color="#fbbf24" />
          <div><strong>Compatibility statement:</strong> macOS and Linux desktop releases are not claimed or validated by this demo.</div>
        </div>
      </div>
    );
  }

  return (
    <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 22 }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <div>
          <div style={{ display: 'flex', alignItems: 'center', gap: 9, color: '#38bdf8', fontWeight: 700, fontSize: 13, textTransform: 'uppercase', letterSpacing: '.08em' }}><Sparkles size={17} /> Aqua Assistant</div>
          <h1 style={{ fontSize: 32, margin: '8px 0 5px' }}>One private desktop assistant, end to end</h1>
          <p style={{ color: 'var(--text-secondary)', margin: 0, fontSize: 16 }}>Voice, automation, knowledge, learning, files, security, and device control.</p>
        </div>
        <div style={{ border: '1px solid rgba(52,211,153,.35)', color: '#6ee7b7', borderRadius: 999, padding: '9px 15px', fontWeight: 700 }}>Local & private by design</div>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 18 }}>
        {groups.map((group) => (
          <section key={group.title} className="glass-panel" style={{ borderRadius: 16, padding: 22, borderTop: `3px solid ${group.accent}` }}>
            <h2 style={{ margin: '0 0 16px', fontSize: 19 }}>{group.title}</h2>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 11 }}>
              {group.items.map(([Icon, title, text]) => {
                const ItemIcon = Icon as typeof Mic2;
                return (
                  <div key={title as string} style={{ display: 'flex', gap: 12, padding: 13, borderRadius: 11, background: 'rgba(255,255,255,.025)', border: '1px solid var(--glass-border)' }}>
                    <ItemIcon size={20} color={group.accent} style={{ flexShrink: 0, marginTop: 2 }} />
                    <div><strong style={{ display: 'block', fontSize: 14 }}>{title as string}</strong><span style={{ color: 'var(--text-muted)', fontSize: 12.5, lineHeight: 1.45 }}>{text as string}</span></div>
                  </div>
                );
              })}
            </div>
          </section>
        ))}
      </div>
    </div>
  );
}
