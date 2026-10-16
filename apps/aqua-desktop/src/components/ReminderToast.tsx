import { useEffect, useSyncExternalStore } from 'react';
import { Bell } from 'lucide-react';
import { kindLabel } from '../announcements';
import { startAnnouncementPlayer, toasts } from '../announcementsPlayer';

/** The time of day a sentence was made, for the toast ("14:05"), or '' when the core did not say. */
function timeOfDay(unixSecs: number | null): string {
  if (unixSecs === null) return '';
  try {
    return new Date(unixSecs * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  } catch {
    return '';
  }
}

/**
 * The sentences Aqua says on its own (a reminder that came due, a timer), shown as a toast and said aloud. Mounting it starts the player,
 * which asks the core for the next sentence every few seconds (src/announcementsPlayer.ts); one toast shows at a time, the others wait,
 * and Dismiss brings up the next. The sentence is a text child, never markup. A toast that is dismissed is not said again, and a sentence
 * that was said is not said twice.
 */
export function ReminderToast() {
  useEffect(() => startAnnouncementPlayer(), []);
  const { current, waiting } = useSyncExternalStore(toasts.subscribe, toasts.getSnapshot);
  if (!current) return null;
  const at = timeOfDay(current.at);
  return (
    <div
      role="status"
      aria-live="polite"
      className="glass-panel"
      style={{
        position: 'fixed', right: 20, bottom: 20, zIndex: 900, width: 'min(380px, calc(100vw - 40px))', padding: 16, borderRadius: 14,
        display: 'flex', flexDirection: 'column', gap: 10, boxShadow: 'var(--shadow-glass)',
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <Bell size={16} color="var(--accent-amber)" />
        <strong style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{kindLabel(current.kind, current.text)}</strong>
        {at && <span style={{ fontSize: 12, color: 'var(--text-muted)', marginLeft: 'auto' }}>{at}</span>}
      </div>
      <p style={{ margin: 0, fontSize: 15, lineHeight: 1.45, color: 'var(--text-primary)', overflowWrap: 'anywhere' }}>{current.text}</p>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
        <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>{waiting > 0 ? (waiting === 1 ? '1 more waiting' : `${waiting} more waiting`) : ''}</span>
        <button type="button" className="btn-ghost" style={{ padding: '4px 12px' }} onClick={() => toasts.dismiss()}>Dismiss</button>
      </div>
    </div>
  );
}
