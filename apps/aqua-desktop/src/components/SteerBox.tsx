import { useCallback, useEffect, useState, type CSSProperties, type FormEvent } from 'react';
import { Send } from 'lucide-react';
import { steerApi, steerProblem, STEER_MAX_CHARS } from '../api/steer';
import { errorSentence } from '../api/plainErrors';
import { BUSY_POLL_MS } from '../api/polling';
import { useVisibleInterval } from '../hooks/useVisibleInterval';
import { useTasks } from '../tasks';

const NOTE_MS = 8000;

const input: CSSProperties = {
  flex: '1 1 260px', minWidth: 0, background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8,
  padding: '8px 10px', color: 'inherit', fontSize: 14, boxSizing: 'border-box',
};

/**
 * One line to say something to the task that is running: it is added to the task at its next step, as your own words, and "stop" ends it. It cannot
 * answer a question Aqua asked and it approves nothing. It is switched off while no task runs (this app's own tasks, or any task the core is serving),
 * and it says "taken" only when the core says so.
 */
export function SteerBox() {
  const { current } = useTasks();
  const [coreBusy, setCoreBusy] = useState(false);
  const [text, setText] = useState('');
  const [sending, setSending] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; sentence: string } | null>(null);

  const readBusy = useCallback(async () => {
    try {
      setCoreBusy(await steerApi.busy());
    } catch {
      setCoreBusy(false);
    }
  }, []);
  useEffect(() => {
    void readBusy();
  }, [readBusy]);
  useVisibleInterval(() => void readBusy(), BUSY_POLL_MS, true);

  // The sentence about the last try goes by itself.
  useEffect(() => {
    if (note === null) return;
    const gone = window.setTimeout(() => setNote(null), NOTE_MS);
    return () => window.clearTimeout(gone);
  }, [note]);

  const running = current !== null || coreBusy;
  const problem = text.trim() === '' ? null : steerProblem(text);

  const send = async (e: FormEvent) => {
    e.preventDefault();
    if (!running || sending || steerProblem(text) !== null) return;
    setSending(true);
    setNote(null);
    try {
      const result = await steerApi.send(text);
      setNote({ ok: true, sentence: result.sentence });
      setText('');
    } catch (error) {
      // The words stay in the box: nothing was taken.
      setNote({ ok: false, sentence: errorSentence(error) });
    } finally {
      setSending(false);
    }
  };

  return (
    <form className="glass-panel" onSubmit={(e) => void send(e)} style={{ padding: 16, borderRadius: 12, display: 'flex', flexDirection: 'column', gap: 8 }}>
      <label htmlFor="steer-input" style={{ fontSize: 13, fontWeight: 600 }}>Add to the task that is running</label>
      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
        <input
          id="steer-input"
          type="text"
          value={text}
          disabled={!running || sending}
          maxLength={STEER_MAX_CHARS * 2}
          autoComplete="off"
          placeholder={running ? 'for example: also add the date. Say stop to end it.' : 'Nothing is running now.'}
          onChange={(e) => setText(e.target.value)}
          style={{ ...input, opacity: running ? 1 : 0.6 }}
        />
        <button type="submit" className="btn-primary" disabled={!running || sending || text.trim() === '' || problem !== null} style={{ display: 'inline-flex', alignItems: 'center', gap: 8, padding: '8px 16px' }}>
          <Send size={14} /> {sending ? 'Sending…' : 'Send'}
        </button>
      </div>
      <p style={{ margin: 0, fontSize: 12.5, color: 'var(--text-secondary)' }}>
        Aqua reads it at its next step, as your own words. It cannot answer a question Aqua asked, and it approves nothing.
      </p>
      {problem !== null && <p role="alert" style={{ margin: 0, fontSize: 13, color: 'var(--state-attention)' }}>{problem}</p>}
      {note !== null && (
        <p role={note.ok ? 'status' : 'alert'} style={{ margin: 0, fontSize: 13, color: note.ok ? 'var(--state-ok)' : 'var(--state-problem)' }}>{note.sentence}</p>
      )}
    </form>
  );
}
