import { useRef } from 'react';
import { Square } from 'lucide-react';
import { stopTask, useTasks } from '../tasks';

/**
 * What the assistant is doing right now, wherever the task was given (admin panel, voice, text), the tasks waiting behind
 * it, and the action to stop it. It folds open only while there is something to show.
 */
export function TaskBar() {
  const { current, queued } = useTasks();
  const open = current !== null || queued.length > 0;
  // Keep the last content while the bar folds shut, so its text does not vanish before it has gone
  const last = useRef({ current, queued });
  if (open) last.current = { current, queued };
  const shown = open ? { current, queued } : last.current;
  const speaking = shown.current?.phase === 'speaking';

  return (
    <div className="collapse" data-open={open} style={{ marginBottom: 0 }}>
      <div>
        <div className="task-bar" role="status">
          <span className="pulse-dot" data-work style={{ width: 8, height: 8, flexShrink: 0, color: speaking ? 'var(--state-speak)' : 'var(--state-think)' }} />
          <span style={{ flex: 1, minWidth: 0 }}>
            {shown.current ? (
              <>
                <strong>{shown.current.label}</strong> {speaking ? 'is speaking' : 'is thinking'}
              </>
            ) : (
              <>Starting the next task…</>
            )}
            {shown.queued.length > 0 && (
              <span style={{ color: 'var(--text-secondary)' }}>
                {' '}· {shown.queued.length} waiting: {shown.queued.map((t) => t.label).join(', ')}
              </span>
            )}
          </span>
          {shown.current && (
            <button className="btn-danger" onClick={stopTask} style={{ display: 'inline-flex', alignItems: 'center', gap: 8, padding: '6px 14px' }}>
              <Square size={13} fill="currentColor" /> {speaking ? 'Stop speaking' : 'Stop thinking'}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
