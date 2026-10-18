import type { ReactNode } from 'react';

export type Tone = 'ok' | 'attention' | 'problem' | 'neutral';

/** Title and one plain sentence under it, with room on the right for the page's main actions. */
export function PageHeader({ title, subtitle, actions }: { title: string; subtitle?: ReactNode; actions?: ReactNode }) {
  return (
    <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 16, flexWrap: 'wrap' }}>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6, minWidth: 0 }}>
        <h2 className="page-title">{title}</h2>
        {subtitle && <p className="page-subtitle">{subtitle}</p>}
      </div>
      {actions && <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap' }}>{actions}</div>}
    </div>
  );
}

const DOT: Record<Tone, string> = {
  ok: 'var(--state-ok)',
  attention: 'var(--state-attention)',
  problem: 'var(--state-problem)',
  neutral: 'var(--text-muted)',
};

/** A small status chip: a coloured dot, a label, and a value. Colour changes ease over, they do not snap. */
export function Pill({ tone = 'neutral', label, value, live, title }: { tone?: Tone; label: string; value?: ReactNode; live?: boolean; title?: string }) {
  return (
    <span className="pill" data-state={tone === 'neutral' ? undefined : tone} title={title}>
      <span className={live ? 'pulse-dot' : undefined} style={{ width: 7, height: 7, borderRadius: '50%', background: DOT[tone], color: DOT[tone], flexShrink: 0, boxShadow: live ? undefined : `0 0 6px ${DOT[tone]}`, transition: 'background 0.4s' }} />
      <span>{label}</span>
      {value !== undefined && <strong>{value}</strong>}
    </span>
  );
}

/** A titled block. The title is a sentence-case heading, not a label. */
export function Section({ title, hint, children, actions }: { title: string; hint?: ReactNode; children: ReactNode; actions?: ReactNode }) {
  return (
    <section className="glass-panel" style={{ padding: 22, display: 'flex', flexDirection: 'column', gap: 16 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: 12, flexWrap: 'wrap' }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <h3 style={{ fontSize: 17, fontWeight: 600 }}>{title}</h3>
          {hint && <p style={{ fontSize: 13.5, color: 'var(--text-secondary)', lineHeight: 1.5, maxWidth: '72ch' }}>{hint}</p>}
        </div>
        {actions}
      </div>
      {children}
    </section>
  );
}

/** A calm, informative first-load state for panels whose local service is not ready yet. */
export function UnavailableState({
  title,
  description,
  status = 'Waiting for the local service',
}: {
  title: string;
  description: ReactNode;
  status?: string;
}) {
  return (
    <div
      className="glass-panel"
      role="status"
      style={{
        minHeight: 190,
        padding: 28,
        display: 'grid',
        placeItems: 'center',
        textAlign: 'center',
        background: 'radial-gradient(circle at 50% 0%, rgba(60, 203, 255, 0.08), transparent 58%), var(--glass-bg)',
      }}
    >
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 10, maxWidth: 560 }}>
        <div style={{ width: 44, height: 44, borderRadius: 14, display: 'grid', placeItems: 'center', background: 'rgba(60, 203, 255, 0.1)', border: '1px solid rgba(60, 203, 255, 0.24)' }}>
          <span className="pulse-dot" style={{ width: 10, height: 10, borderRadius: '50%', background: 'var(--accent-cyan)', color: 'var(--accent-cyan)' }} />
        </div>
        <h3 style={{ fontSize: 18 }}>{title}</h3>
        <p style={{ color: 'var(--text-secondary)', fontSize: 14, lineHeight: 1.55 }}>{description}</p>
        <Pill tone="attention" label={status} live />
        <span style={{ color: 'var(--text-muted)', fontSize: 12 }}>This panel reconnects automatically. No refresh is required.</span>
      </div>
    </div>
  );
}

/** A checkbox with a label and an optional one-line explanation under it. */
export function Switch({ label, hint, checked, onChange, disabled }: { label: string; hint?: string; checked: boolean; onChange: (v: boolean) => void; disabled?: boolean }) {
  return (
    <label style={{ display: 'flex', gap: 10, alignItems: 'flex-start', fontSize: 14, opacity: disabled ? 0.5 : 1 }}>
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} style={{ accentColor: 'var(--accent-cyan)', marginTop: 3 }} />
      <span>
        {label}
        {hint && <span style={{ display: 'block', fontSize: 13, color: 'var(--text-secondary)' }}>{hint}</span>}
      </span>
    </label>
  );
}
