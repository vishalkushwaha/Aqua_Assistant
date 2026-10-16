import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { canType, EDITED_LINE, modeText, typeLabel, type DictationMode, type DiffPart } from '../api/dictation';
import { alertText, muted, textBox } from './screenStyles';

export interface DictationPreviewProps {
  /** `dictation` (a clip turned into text) or `rewrite` (command mode: a selection rewritten as the person said). */
  mode?: DictationMode;
  /** The text the core produced. The person may change it in the box before it is typed. */
  text: string;
  /** What the speech model wrote before the tidy-up; shown only when it differs from `text`. */
  heard?: string;
  /** What the tidy-up (or the rewrite) changed, one line each (see `tidyUpLines` and `rewriteLines` in api/dictation). */
  changes?: string[];
  /** The words that change in a rewrite. */
  diff?: DiffPart[];
  /** True while the shell is typing: both buttons wait. */
  busy?: boolean;
  /** A sentence about why the last try to type did not work. */
  error?: string | null;
  /** The person chose to type the text, as it is in the box now. Nothing is typed by this component: the shell does it. */
  onType: (text: string) => void;
  /** The person chose to throw the text away. */
  onDiscard: () => void;
}

const PART_STYLE = {
  same: {},
  delete: { color: 'var(--state-problem)', textDecoration: 'line-through' },
  insert: { color: 'var(--state-ok)', textDecoration: 'underline' },
} as const;
const PART_TITLE = { same: undefined, delete: 'taken out', insert: 'put in' } as const;

/**
 * The popup that shows the text a dictation produced before it is typed: the text in a box that can be edited, the mode, what the tidy-up
 * changed, and two choices, Type it and Discard. It is pure: the text and the two callbacks come in as props, and nothing is typed,
 * stored or sent from here. Ctrl+Enter types and Escape discards. Everything that comes from the core is a text child, never markup.
 */
export function DictationPreview({ mode = 'dictation', text, heard, changes = [], diff = [], busy = false, error = null, onType, onDiscard }: DictationPreviewProps) {
  const [draft, setDraft] = useState(text);
  const box = useRef<HTMLTextAreaElement>(null);

  // A new text from the core replaces what is in the box.
  useEffect(() => {
    setDraft(text);
  }, [text]);
  // The box has the focus when the popup opens, so a stray Enter adds a line and never types.
  useEffect(() => {
    box.current?.focus();
  }, []);

  const edited = draft !== text;
  const ready = canType(draft) && !busy;
  const lines = edited ? [...changes, EDITED_LINE] : changes;

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      onDiscard();
    } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && ready) {
      e.preventDefault();
      onType(draft);
    }
  };

  return (
    <div role="dialog" aria-label={modeText(mode)} className="glass-panel" style={{ padding: 20, borderRadius: 12, display: 'flex', flexDirection: 'column', gap: 12, maxWidth: 560 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', gap: 12, flexWrap: 'wrap' }}>
        <strong style={{ fontSize: 14 }}>{modeText(mode)}</strong>
        <span style={muted}>Check the text. Nothing is typed until you choose.</span>
      </div>

      {mode === 'rewrite' && diff.length > 0 && (
        <div aria-label="What changes" style={{ fontSize: 13.5, lineHeight: 1.6, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', userSelect: 'text' }}>
          {diff.map((p, i) => (
            <span key={i} style={PART_STYLE[p.op]} title={PART_TITLE[p.op]}>{p.text}</span>
          ))}
        </div>
      )}

      <textarea
        ref={box}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={onKeyDown}
        aria-label="The text that will be typed"
        placeholder="No text."
        rows={Math.min(10, Math.max(3, draft.split('\n').length + 1))}
        style={textBox}
      />

      {heard !== undefined && heard.trim() !== '' && heard !== text && (
        <div style={muted}>As heard: {heard}</div>
      )}
      {lines.map((l, i) => (
        <div key={i} style={muted}>{l}</div>
      ))}
      {error && <div role="alert" style={alertText}>{error}</div>}

      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <button type="button" className="btn-primary" disabled={!ready} onClick={() => onType(draft)}>{typeLabel(mode)}</button>
        <button type="button" className="btn-ghost" disabled={busy} onClick={onDiscard}>Discard</button>
      </div>
    </div>
  );
}
