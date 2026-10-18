import { useEffect, useRef, useState } from 'react';
import { openEventStream } from './stream';
import { readResponseJson, requireJson, responseErrorMessage } from './jsonResponse';

export type DeviceType =
  | 'keyboard' | 'mouse' | 'headphones' | 'speaker' | 'microphone' | 'controller' | 'camera' | 'printer'
  | 'usb_storage' | 'phone' | 'bluetooth_other' | 'usb_unknown' | 'computer' | 'unknown';

export type Category =
  | 'keyboards' | 'mice' | 'headphones' | 'speakers' | 'microphones' | 'controllers' | 'cameras' | 'printers'
  | 'storage' | 'phones' | 'computer' | 'other';

export const CATEGORY_LABELS: Record<Category, string> = {
  keyboards: 'Keyboards', mice: 'Mice', headphones: 'Headphones and earbuds', speakers: 'Speakers', microphones: 'Microphones',
  controllers: 'Game controllers', cameras: 'Cameras', printers: 'Printers', storage: 'USB storage', phones: 'Phones',
  computer: "This computer's battery", other: 'Other devices',
};

export interface DevicePrefs {
  ignore: boolean;
  connect: boolean | null;
  disconnect: boolean | null;
  battery: boolean | null;
  charging: boolean | null;
}

export interface DeviceView {
  ref: string;
  name: string;
  device_type: DeviceType;
  type_text: string;
  connection: 'usb' | 'bluetooth' | 'wired' | 'internal' | 'unknown';
  id_kind: 'address' | 'usb_serial' | 'container' | 'weak' | 'system';
  connected: boolean;
  battery_percent: number | null;
  charging: 'unknown' | 'charging' | 'not_charging' | 'full' | 'discharging';
  battery_source: string;
  /** ok | low | very_low | critical | not_reported | unknown */
  battery_state: string;
  last_seen: number;
  prefs: DevicePrefs;
  same_name_as_another: boolean;
}

export interface DeviceSettings {
  enabled: boolean;
  announce_connections: boolean;
  announce_disconnections: boolean;
  announce_low_battery: boolean;
  announce_critical_battery: boolean;
  announce_charging: boolean;
  announce_fully_charged: boolean;
  announce_recovered: boolean;
  announce_unknown_usb: boolean;
  low_percent: number;
  very_low_percent: number;
  critical_percent: number;
  battery_cooldown_secs: number;
  charging_cooldown_secs: number;
  rate_limit_per_minute: number;
  infer_charging: boolean;
  repeat_critical_minutes: number;
  quiet_hours: { enabled: boolean; start_hour: number; end_hour: number };
  disabled_categories: Category[];
}

export interface Decision {
  at_ms: number;
  device: string;
  kind: string | null;
  result: 'announced' | 'suppressed' | 'info';
  why: string;
}

export interface DevicesSnapshot {
  version: number;
  now_ms: number;
  settings: DeviceSettings;
  devices: DeviceView[];
  pending: number;
  sources: Record<string, { state: string; reason?: string }>;
  recent: Decision[];
  counters: { events: number; announced: number; suppressed: number; expired: number };
  warning: string | null;
  cpu_seconds?: { engine: number; source: number };
}

async function send(path: string, body?: unknown): Promise<{ ok: boolean; data?: unknown; error?: string }> {
  try {
    const res = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
    if (res.status === 204) return { ok: true };
    const data = await readResponseJson(res);
    return res.ok ? { ok: true, data: data ?? undefined } : { ok: false, error: responseErrorMessage(data) || `HTTP ${res.status}` };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export const devicesApi = {
  async get(): Promise<DevicesSnapshot | null> {
    try {
      const res = await fetch('/api/v1/devices', { headers: { Accept: 'application/json' } });
      return res.ok ? await requireJson<DevicesSnapshot>(res) : null;
    } catch {
      return null;
    }
  },
  settings: (patch: Partial<DeviceSettings>) => send('/api/v1/devices/settings', patch),
  deviceSettings: (ref: string, prefs: DevicePrefs) => send(`/api/v1/devices/${ref}/settings`, prefs),
  forget: (ref: string) => send(`/api/v1/devices/${ref}/forget`),
  forgetAll: () => send('/api/v1/devices/forget-all'),
  test: () => send('/api/v1/devices/test'),
  next: (allow: 'all' | 'critical_only') => send('/api/v1/devices/announcements/next', { allow }),
  ack: (id: number, spoken: boolean) => send(`/api/v1/devices/announcements/${id}/ack`, { spoken }),
};

/** The device state, live: a stream of snapshots, with a slow poll as the fallback when the stream is not available. */
export function useDevices(): DevicesSnapshot | null {
  const [snap, setSnap] = useState<DevicesSnapshot | null>(null);
  const lastVersion = useRef(0);
  useEffect(() => {
    let closed = false;
    let stream: { close: () => void } | null = null;
    let poll: number | undefined;
    const startPolling = () => {
      if (poll !== undefined) return;
      const once = async () => {
        const s = await devicesApi.get();
        if (s && !closed) setSnap(s);
      };
      void once();
      poll = window.setInterval(() => void once(), 3000);
    };
    stream = openEventStream('/api/v1/devices/stream', {
      onEvent: (name, data) => {
        if (name !== 'snapshot') return;
        try {
          const s = JSON.parse(data) as DevicesSnapshot;
          if (s.version !== lastVersion.current) {
            lastVersion.current = s.version;
            setSnap(s);
          }
        } catch {
          /* a damaged message is skipped; the next snapshot replaces it */
        }
      },
      onEnd: startPolling,
      onError: startPolling,
    });
    return () => {
      closed = true;
      stream?.close();
      if (poll !== undefined) window.clearInterval(poll);
    };
  }, []);
  return snap;
}
