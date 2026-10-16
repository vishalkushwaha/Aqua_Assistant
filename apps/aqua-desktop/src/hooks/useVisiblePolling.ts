import { useEffect, useRef } from 'react';

/** The fastest a screen of the app asks the core again and again: once a second. */
export const MIN_POLL_MS = 1000;

/** The pace asked for, but never faster than once a second. */
export function pollInterval(requestedMs: number): number {
  return Number.isFinite(requestedMs) ? Math.max(MIN_POLL_MS, Math.floor(requestedMs)) : MIN_POLL_MS;
}

/**
 * Calls `tick` every `intervalMs` while the screen that uses this is mounted and the window is in view, and once when the window comes
 * back into view. A screen that is not shown is not mounted (the app renders the page that is open), and a window that is hidden asks
 * for nothing. The first call is the screen's own (it loads when it mounts).
 */
export function useVisiblePolling(tick: () => void, intervalMs: number): void {
  const latest = useRef(tick);
  latest.current = tick;
  useEffect(() => {
    const id = window.setInterval(() => {
      if (!document.hidden) latest.current();
    }, pollInterval(intervalMs));
    const onVisible = () => {
      if (!document.hidden) latest.current();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      window.clearInterval(id);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [intervalMs]);
}
