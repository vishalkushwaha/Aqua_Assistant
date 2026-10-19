import {
  Bot, Brain, Database, FileSearch, GitBranch, Headphones, Laptop,
  LockKeyhole, Mic2, Network, ShieldCheck, Sparkles, Vault, Workflow,
} from 'lucide-react';

type ShowcaseVariant = 'capabilities' | 'platform' | 'local-ai' | 'voice' | 'automation' | 'knowledge';

const detailedShowcases = {
  'local-ai': {
    eyebrow: 'Private intelligence pipeline',
    title: 'Local LLM: from private input to controlled action',
    subtitle: 'Aqua runs a GGUF language model through its local Llama runtime, streams the response, and keeps tool execution behind deterministic policy and approval checks.',
    badge: 'ON-DEVICE BY DEFAULT',
    steps: ['Voice or text input', 'Whisper transcription', 'Local GGUF reasoning', 'Policy-gated tools', 'UI and Kokoro speech'],
    items: [
      [Brain, 'Local model runtime', 'Role-aware model selection, streaming generation, bounded context, and optional CPU or GPU acceleration.'],
      [Database, 'Grounded context', 'Private memory, indexed files, and FalkorDB evidence are retrieved before the model composes an answer.'],
      [ShieldCheck, 'Controlled execution', 'The model can propose a tool call, but ToolHost validates scope, asks for approval when required, and records the result.'],
      [Mic2, 'Offline voice loop', 'DeepFilterNet cleans audio, Whisper converts speech to text, the local LLM reasons, and Kokoro speaks the answer.'],
    ],
    footer: 'Prompts and outputs stay local unless the user explicitly authorizes a network-capable tool or configures a remote FalkorDB endpoint.',
  },
  voice: {
    eyebrow: 'Natural desktop interaction',
    title: 'Voice, security, devices, and the floating orb',
    subtitle: 'Aqua combines hands-free input, local speech processing, identity checks, and an always-available desktop surface.',
    badge: 'VOICE + DESKTOP',
    steps: ['Wake phrase', 'Noise cleanup', 'Speech recognition', 'Identity and policy', 'Spoken response'],
    items: [
      [Mic2, 'Voice profiles', 'Select local Kokoro voices, preview delivery, and tune the spoken assistant experience.'],
      [LockKeyhole, 'Voice security', 'Enroll a master voice, configure a custom wake phrase, and require biometric verification for protected actions.'],
      [Headphones, 'Audio devices', 'Choose microphone and speaker routes, monitor device state, and control announcements.'],
      [Bot, 'Floating neural orb', 'Use the always-on-top orb for wake, listening, thinking, speaking, screen context, and quick access to the full panel.'],
    ],
    footer: 'Voice detection and speech processing are designed to run locally, with clear standby, listening, working, and speaking states.',
  },
  automation: {
    eyebrow: 'Safe automation',
    title: 'Workflows, scheduled routines, tools, and approvals',
    subtitle: 'Aqua turns repeatable work into inspectable steps while keeping sensitive actions inside explicit limits.',
    badge: 'POLICY CONTROLLED',
    steps: ['Describe a goal', 'Compose steps', 'Validate permissions', 'Run or schedule', 'Inspect the audit trail'],
    items: [
      [Workflow, 'Workflows', 'Build reusable sequences from tool calls, agent goals, and saved values, with per-step status and retry visibility.'],
      [GitBranch, 'Automated routines', 'Generate flows from natural language, start from templates, schedule runs, and inspect failures or artifacts.'],
      [ShieldCheck, 'App approvals', 'Review tool policy, available tools, recent activity, and stop active agent work from one control surface.'],
      [LockKeyhole, 'Permissions and privacy', 'Control project trust, browser profiles, consent, autonomy level, executable access, and reversible approvals.'],
    ],
    footer: 'The language model suggests a plan; deterministic rules, scopes, confirmations, and run limits decide what can actually happen.',
  },
  knowledge: {
    eyebrow: 'Private knowledge system',
    title: 'Memory, learning, files, skills, vault, and graphs',
    subtitle: 'Aqua keeps useful context inspectable and removable instead of hiding it inside an opaque assistant history.',
    badge: 'INSPECTABLE + REVERSIBLE',
    steps: ['Capture context', 'Verify evidence', 'Store by scope', 'Recall when relevant', 'Correct or forget'],
    items: [
      [Brain, 'Memory and learning', 'Review remembered facts, preferences, corrections, verified experiences, retrieval limits, and self-improvement controls.'],
      [Network, 'FalkorDB memory graph', 'Explore associative memory, company knowledge, multi-agent state, graph algorithms, and Cypher queries.'],
      [FileSearch, 'Files and reviewed skills', 'Index local files for private retrieval and approve proposed skills before they become available to the agent.'],
      [Vault, 'Secure Vault', 'Keep selected personal documents in a protected context store with explicit import, search, and removal controls.'],
    ],
    footer: 'Users can inspect, correct, disable, retain, or delete knowledge; tenant identifiers keep organizational graph records separated.',
  },
} as const;

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

export function ProductShowcasePage({ variant }: { variant: ShowcaseVariant }) {
  if (variant in detailedShowcases) {
    const detail = detailedShowcases[variant as keyof typeof detailedShowcases];
    return (
      <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 22 }}>
        <div className="glass-panel" style={{ padding: '28px 32px', borderRadius: 18, border: '1px solid rgba(56,189,248,.28)', background: 'linear-gradient(135deg, rgba(14,165,233,.10), rgba(139,92,246,.08))' }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', gap: 18, alignItems: 'flex-start', flexWrap: 'wrap' }}>
            <div style={{ maxWidth: 1040 }}>
              <div style={{ color: '#38bdf8', fontWeight: 800, letterSpacing: '.09em', textTransform: 'uppercase', fontSize: 12 }}>{detail.eyebrow}</div>
              <h1 style={{ margin: '8px 0 9px', fontSize: 34 }}>{detail.title}</h1>
              <p style={{ color: 'var(--text-secondary)', fontSize: 16, lineHeight: 1.6, margin: 0 }}>{detail.subtitle}</p>
            </div>
            <div style={{ border: '1px solid rgba(52,211,153,.35)', color: '#6ee7b7', borderRadius: 999, padding: '9px 14px', fontSize: 12, fontWeight: 800 }}>{detail.badge}</div>
          </div>
        </div>

        <div className="glass-panel" style={{ padding: '18px 22px', borderRadius: 15 }}>
          <div style={{ color: 'var(--text-muted)', fontWeight: 700, fontSize: 11, letterSpacing: '.08em', marginBottom: 12 }}>INPUT TO OUTPUT</div>
          <div style={{ display: 'grid', gridTemplateColumns: `repeat(${detail.steps.length}, minmax(0, 1fr))`, gap: 10 }}>
            {detail.steps.map((step, index) => (
              <div key={step} style={{ position: 'relative', padding: '13px 12px', borderRadius: 10, background: 'rgba(56,189,248,.06)', border: '1px solid rgba(56,189,248,.18)', textAlign: 'center', fontSize: 12.5, fontWeight: 700 }}>
                <span style={{ color: '#38bdf8', marginRight: 6 }}>{index + 1}.</span>{step}
              </div>
            ))}
          </div>
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: 16 }}>
          {detail.items.map(([Icon, title, text]) => {
            const CardIcon = Icon as typeof Brain;
            return (
              <section key={title} className="glass-panel" style={{ padding: 20, borderRadius: 14, display: 'flex', gap: 15, minHeight: 118 }}>
                <div style={{ width: 42, height: 42, display: 'grid', placeItems: 'center', borderRadius: 11, background: 'rgba(56,189,248,.10)', flexShrink: 0 }}><CardIcon size={22} color="#38bdf8" /></div>
                <div><h2 style={{ margin: '1px 0 7px', fontSize: 18 }}>{title}</h2><p style={{ color: 'var(--text-secondary)', lineHeight: 1.52, margin: 0, fontSize: 13.5 }}>{text}</p></div>
              </section>
            );
          })}
        </div>

        <div className="glass-panel" style={{ padding: '16px 20px', borderRadius: 13, display: 'flex', gap: 12, alignItems: 'center', color: 'var(--text-secondary)' }}>
          <ShieldCheck size={22} color="#34d399" style={{ flexShrink: 0 }} /><span>{detail.footer}</span>
        </div>
      </div>
    );
  }

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
