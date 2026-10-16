import { useEffect, useRef } from 'react';
import { MIN_POLL_MS } from '../api/polling';

/**
 * Calls `callback` every `ms` (never faster than once a second) while `active` is true and the window is visible. It stops when the component that
 * uses it is closed. The newest `callback` is always the one called, so it may use the latest state without being listed anywhere.
 */
export function useVisibleInterval(callback: () => void, ms: number, active: boolean): void {
  const latest = useRef(callback);
  useEffect(() => {
    latest.current = callback;
  });
  useEffect(() => {
    if (!active) return;
    const id = window.setInterval(() => {
      if (!document.hidden) latest.current();
    }, Math.max(MIN_POLL_MS, ms));
    return () => window.clearInterval(id);
  }, [ms, active]);
}
