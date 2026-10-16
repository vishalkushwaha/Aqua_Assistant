// Server-sent events over fetch. The browser's EventSource cannot send an Authorization header, which the packaged
// app needs for every gateway request; a fetch (which the gateway bridge gives the token to) can. Same message
// format: "event: name" and "data: ..." lines, a blank line between events.

export interface EventStreamHandlers {
  onEvent: (name: string, data: string) => void;
  /** The server finished the stream normally. */
  onEnd?: () => void;
  /** The connection failed or ended without the server finishing; the caller can fall back to polling. */
  onError?: (e: unknown) => void;
}

/** Split a chunk of SSE text into complete events and the unfinished rest. Exported for testing. */
export function parseEvents(buffer: string): { events: { name: string; data: string }[]; rest: string } {
  const events: { name: string; data: string }[] = [];
  const blocks = buffer.replace(/\r\n/g, '\n').split('\n\n');
  const rest = blocks.pop() ?? '';
  for (const block of blocks) {
    let name = 'message';
    const data: string[] = [];
    for (const line of block.split('\n')) {
      if (line.startsWith('event:')) name = line.slice(6).trim();
      else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
    }
    if (data.length || name !== 'message') events.push({ name, data: data.join('\n') });
  }
  return { events, rest };
}

export function openEventStream(url: string, h: EventStreamHandlers): { close: () => void } {
  const controller = new AbortController();
  void (async () => {
    try {
      const res = await fetch(url, { headers: { Accept: 'text/event-stream' }, signal: controller.signal });
      if (!res.ok || !res.body) throw new Error(`stream ${res.status}`);
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        const parsed = parseEvents(buffer);
        buffer = parsed.rest;
        for (const ev of parsed.events) h.onEvent(ev.name, ev.data);
      }
      h.onEnd?.();
    } catch (e) {
      if (!controller.signal.aborted) h.onError?.(e);
    }
  })();
  return { close: () => controller.abort() };
}
