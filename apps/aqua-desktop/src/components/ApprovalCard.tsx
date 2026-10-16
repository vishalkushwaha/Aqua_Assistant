import { useEffect, useRef, type CSSProperties, type ReactNode } from 'react';
import { ShieldAlert, Globe, Wrench } from 'lucide-react';
import {
  answerHint,
  answerLabel,
  choicesFor,
  classWords,
  isSerious,
  isTooLate,
  NO_DESCRIPTION,
  runOptionFor,
  titleFor,
  waitingSentence,
  type ApprovalAnswer,
  type ApprovalQuestion,
  type ApprovalScope,
} from '../api/approvals';

const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const label: CSSProperties = { fontSize: 11, fontWeight: 700, letterSpacing: 0.4, textTransform: 'uppercase', color: 'var(--text-muted)' };
const codeStyle: CSSProperties = { fontSize: 12.5, padding: '2px 8px', borderRadius: 6, background: 'rgba(255,255,255,0.08)', wordBreak: 'break-all' };

function btn(background: string, color: string): CSSProperties {
  return { padding: '9px 16px', borderRadius: 8, border: '1px solid var(--glass-border)', background, color, fontWeight: 600, cursor: 'pointer' };
}

// The run option (card OA6-05) is an outline in the amber the card already uses for what needs care: not the cyan of "approve once", not the red of "deny", and no fill, so that it is
// not the button the eye lands on out of habit.
const RUN_OPTION_STYLE: CSSProperties = { ...btn('transparent', '#fbbf24'), border: '1px solid rgba(245,158,11,0.6)' };

const BUTTON_STYLE: Record<ApprovalAnswer, CSSProperties> = {
  deny: btn('rgba(248,113,113,0.14)', '#fca5a5'),
  approve_always: btn('rgba(255,255,255,0.06)', 'var(--text-primary)'),
  approve: btn('rgba(6,182,212,0.18)', 'var(--accent-cyan)'),
};

const RUN_OPTION_ID = 'browser-confirm-run';
const RUN_SENTENCE_ID = 'browser-confirm-run-sentence';

// The ids the buttons have always had.
const BUTTON_ID: Record<ApprovalAnswer, (q: ApprovalQuestion) => string> = {
  deny: () => 'browser-confirm-deny',
  approve_always: (q) => (q.kind === 'site' ? 'browser-confirm-always' : 'browser-confirm-session'),
  approve: () => 'browser-confirm-allow',
};

function Field({ name, children }: { name: string; children: ReactNode }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
      <span style={label}>{name}</span>
      {children}
    </div>
  );
}

export interface ApprovalCardProps {
  question: ApprovalQuestion;
  /** The clock, in unix milliseconds: given by the parent, which ticks once a second while a question is on screen. */
  nowMs: number;
  /** How many more questions wait behind this one. */
  queued: number;
  busy: boolean;
  /** One sentence about the last try to answer this question, or null. */
  error: string | null;
  /** The core said this question cannot be answered any more (answered already, too late, taken back, not there). */
  closed: boolean;
  /** One sentence about the answer given just before this question appeared, or null. */
  notice: string | null;
  /** The scope is given for the run option only (`'run'`, with `'approve'`); every other answer comes with none. */
  onAnswer: (answer: ApprovalAnswer, scope?: ApprovalScope) => void;
  onClose: () => void;
}

/**
 * One question waiting for the person: the exact action and its target, what it can do, the rule that made it ask, what the run has read and
 * holds, and how long an answer is still in time. A question that carries no explanation (an older core) shows the plain text it always had.
 * At zero the card says it is too late and offers only "Close". "Always" is offered only where the question allows it (`choicesFor`).
 * Everything that comes from the core is a text child, never markup.
 * A question that carries the core's `run_grant` sentence (card OA6-05) has a fourth answer, "allow edits in this folder for this run", below the others and after "approve once"
 * in the tab order. It is never the answer the keyboard starts on, it has no key of its own, and the sentence about what it allows is the text beside it.
 */
export function ApprovalCard({ question: q, nowMs, queued, busy, error, closed, notice, onAnswer, onClose }: ApprovalCardProps) {
  const tooLate = closed || isTooLate(q, nowMs);
  const Icon = q.kind === 'site' ? Globe : q.kind === 'tool' ? Wrench : ShieldAlert;
  // Where the keyboard starts: on the answer that does nothing (or, when it is too late, on Close), so that a stray Enter or Space cannot approve.
  const safe = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    safe.current?.focus();
  }, [tooLate]);

  const choices = choicesFor(q);
  const runOption = runOptionFor(q);

  return (
    <div className="glass-panel" style={{ width: 'min(560px, 100%)', maxHeight: 'calc(100vh - 32px)', overflowY: 'auto', borderRadius: 14, padding: 24, display: 'flex', flexDirection: 'column', gap: 14 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <Icon size={22} color={q.kind === 'site' ? 'var(--accent-cyan)' : '#f59e0b'} />
        <h3 id="browser-confirm-title" style={{ fontSize: 17, fontWeight: 700, margin: 0 }}>{titleFor(q)}</h3>
      </div>

      {notice && <p role="status" style={{ margin: 0, fontSize: 12.5, color: 'var(--state-ok)' }}>{notice}</p>}

      <Field name="What it wants to do">
        <p style={{ margin: 0, fontSize: 15, color: q.reason !== '' ? 'var(--text-primary)' : '#fbbf24', lineHeight: 1.5, wordBreak: 'break-word' }}>{q.reason !== '' ? q.reason : NO_DESCRIPTION}</p>
      </Field>

      {(q.tool !== '' || q.url !== null || q.site !== null) && (
        <Field name="Where">
          <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', alignItems: 'center' }}>
            {q.tool !== '' && <code style={codeStyle}>{q.tool}</code>}
            {q.url !== null && <code style={{ ...codeStyle, color: 'var(--text-secondary)' }}>{q.url}</code>}
            {q.url === null && q.site !== null && <code style={{ ...codeStyle, color: 'var(--text-secondary)' }}>{q.site}</code>}
          </div>
        </Field>
      )}

      {q.classes.length > 0 && (
        <Field name="What it can do">
          <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', alignItems: 'center' }}>
            {q.classes.map((c) => (
              <span
                key={c}
                title={c}
                style={{
                  fontSize: 11.5, fontWeight: 700, padding: '2px 8px', borderRadius: 999, letterSpacing: 0.2,
                  background: isSerious(c) ? 'rgba(245,158,11,0.18)' : 'rgba(6,182,212,0.14)',
                  color: isSerious(c) ? '#fbbf24' : 'var(--accent-cyan)',
                }}
              >
                {classWords(c)}
              </span>
            ))}
          </div>
          {q.risk !== '' && <span style={{ fontSize: 13, color: 'var(--text-primary)', lineHeight: 1.45 }}>{q.risk}</span>}
        </Field>
      )}
      {q.classes.length === 0 && q.risk !== '' && (
        <Field name="What could go wrong">
          <span style={{ fontSize: 13, color: 'var(--text-primary)', lineHeight: 1.45 }}>{q.risk}</span>
        </Field>
      )}

      {q.rule !== '' && (
        <Field name="Why Aqua is asking">
          <span style={{ fontSize: 13, color: 'var(--text-primary)', lineHeight: 1.45 }}>{q.rule}</span>
        </Field>
      )}

      {q.data.length > 0 && (
        <Field name="What is involved">
          <ul style={{ margin: 0, paddingLeft: 18, display: 'flex', flexDirection: 'column', gap: 3 }}>
            {q.data.map((d, i) => (
              <li key={i} style={{ fontSize: 13, color: 'var(--text-primary)', lineHeight: 1.45, wordBreak: 'break-word' }}>{d}</li>
            ))}
          </ul>
        </Field>
      )}

      {q.alwaysAsk && <p style={{ margin: 0, fontSize: 12.5, color: '#fbbf24' }}>This kind of action is asked every time; your answer is not remembered.</p>}

      {q.runId !== null && <p style={{ margin: 0, ...muted }}>This is about the run {q.runId}. The Runs page shows what it has done so far.</p>}

      <p role="timer" style={{ margin: 0, fontSize: 13, color: tooLate ? '#fbbf24' : 'var(--text-secondary)' }}>
        {closed && !isTooLate(q, nowMs) ? 'This question cannot be answered any more.' : waitingSentence(q, nowMs)}
        {queued > 0 && !tooLate ? ` ${queued} more ${queued === 1 ? 'question waits' : 'questions wait'} behind it.` : ''}
      </p>

      {error && <p role="alert" style={{ margin: 0, fontSize: 13, color: '#f87171' }}>{error}</p>}

      {tooLate ? (
        <div style={{ display: 'flex', gap: 10, justifyContent: 'flex-end' }}>
          <button ref={safe} type="button" id="browser-confirm-close" onClick={onClose} style={btn('rgba(255,255,255,0.06)', 'var(--text-primary)')}>
            Close
          </button>
        </div>
      ) : (
        <>
          <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', justifyContent: 'flex-end' }}>
            {choices.map((choice) => (
              <button
                key={choice}
                ref={choice === 'deny' ? safe : undefined}
                type="button"
                id={BUTTON_ID[choice](q)}
                disabled={busy}
                onClick={() => onAnswer(choice)}
                style={BUTTON_STYLE[choice]}
              >
                {answerLabel(q, choice)}
              </button>
            ))}
          </div>
          {runOption !== null && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 8, alignItems: 'flex-end', padding: 12, borderRadius: 10, border: '1px dashed rgba(245,158,11,0.45)' }}>
              <button
                type="button"
                id={RUN_OPTION_ID}
                disabled={busy}
                aria-describedby={RUN_SENTENCE_ID}
                onClick={() => onAnswer('approve', 'run')}
                style={RUN_OPTION_STYLE}
              >
                {runOption.label}
              </button>
              <p id={RUN_SENTENCE_ID} style={{ margin: 0, alignSelf: 'stretch', fontSize: 13, color: 'var(--text-primary)', lineHeight: 1.45, wordBreak: 'break-word' }}>{runOption.sentence}</p>
            </div>
          )}
          <ul style={{ margin: 0, padding: 0, listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 2 }}>
            {choices.map((choice) => (
              <li key={choice} style={muted}>
                <strong style={{ color: 'var(--text-primary)', fontWeight: 600 }}>{answerLabel(q, choice)}:</strong> {answerHint(q, choice)}
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
