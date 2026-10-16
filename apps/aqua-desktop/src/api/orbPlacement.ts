import { useCallback, useEffect, useState } from 'react';

// The orb is moved by the desktop shell (it owns the window), so this page talks to the shell, not to Aqua's backend.
type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

function shellInvoke(): Invoke | null {
  const internals = (window as unknown as { __TAURI_INTERNALS__?: { invoke?: Invoke } }).__TAURI_INTERNALS__;
  return internals?.invoke ?? null;
}

export type Sensitivity = 'low' | 'medium' | 'high';

export interface OrbPlacementSettings {
  auto_move: boolean;
  lock: boolean;
  avoid_content: boolean;
  avoid_cursor: boolean;
  per_app: boolean;
  per_page: boolean;
  scroll_move: boolean;
  scroll_live: boolean;
  scroll_interval_ms: number;
  sensitivity: Sensitivity;
}

export interface OrbLogEntry {
  /** Seconds since 1970. */
  t: number;
  kind: 'moved' | 'kept' | 'saved' | 'skipped' | 'setting' | 'forgot' | 'error';
  text: string;
}

export interface OrbPlacementState {
  supported: boolean;
  settings: OrbPlacementSettings;
  /** "bottom-right"... or null if the person never chose one. */
  preferred: string | null;
  apps: { app: string; spot: string }[];
  pages: number;
  status: {
    detection: string;
    app: string;
    fullscreen: boolean;
    capture: string;
    capture_ms: number;
    note: string | null;
    /** The last time the orb was left alone without looking at a window, and why. */
    last_skip: { t: number; text: string } | null;
    orb: { x: number; y: number; visible: boolean } | null;
  };
  log: OrbLogEntry[];
  counters: { events: number; evaluations: number; captures: number; moves: number; kept: number; skipped: number; scroll_checks: number; scroll_wide: number };
  cpu_ms: { worker: number; hooks: number };
  uptime_s: number;
}

export type SpotName = 'top-left' | 'top-center' | 'top-right' | 'middle-left' | 'middle-right' | 'bottom-left' | 'bottom-center' | 'bottom-right' | 'default';

export const orbPlacement = {
  available: () => shellInvoke() !== null,
  state: async (): Promise<OrbPlacementState> => {
    const invoke = shellInvoke();
    if (!invoke) throw new Error('This page works in the Aqua desktop app.');
    return (await invoke('orb_place_state')) as OrbPlacementState;
  },
  set: async (patch: Partial<OrbPlacementSettings>): Promise<OrbPlacementState> => {
    const invoke = shellInvoke();
    if (!invoke) throw new Error('This page works in the Aqua desktop app.');
    return (await invoke('orb_place_set', { patch })) as OrbPlacementState;
  },
  move: async (spot: SpotName): Promise<void> => {
    await shellInvoke()?.('orb_place_move', { spot });
  },
  forget: async (app?: string): Promise<OrbPlacementState> => {
    const invoke = shellInvoke();
    if (!invoke) throw new Error('This page works in the Aqua desktop app.');
    return (await invoke('orb_place_forget', { app: app ?? null })) as OrbPlacementState;
  },
  recheck: async (): Promise<void> => {
    await shellInvoke()?.('orb_place_recheck');
  },
};

/** The placement state, refreshed every 2 seconds while the page is open and visible. */
export function useOrbPlacement(): { state: OrbPlacementState | null; error: string | null; setState: (s: OrbPlacementState) => void; refresh: () => void } {
  const [state, setState] = useState<OrbPlacementState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refresh = useCallback(() => {
    orbPlacement.state().then(
      (s) => {
        setState(s);
        setError(null);
      },
      (e: unknown) => setError(e instanceof Error ? e.message : String(e)),
    );
  }, []);
  useEffect(() => {
    refresh();
    // Only while this window is the one being used: a page that is open behind other windows has nobody looking at it.
    const id = window.setInterval(() => {
      if (document.visibilityState === 'visible' && document.hasFocus()) refresh();
    }, 2000);
    window.addEventListener('focus', refresh);
    return () => {
      window.clearInterval(id);
      window.removeEventListener('focus', refresh);
    };
  }, [refresh]);
  return { state, error, setState, refresh };
}
