import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * Live microphone level (0..1) and elapsed recording time, for pages that record.
 * Call start(stream) when recording begins and stop() when it ends; leaving the page stops it too.
 * It only listens to the stream it is given: it does not open the microphone itself.
 */
export function useMicMeter() {
  const [level, setLevel] = useState(0);
  const [elapsed, setElapsed] = useState(0);
  const current = useRef<{ stop: () => void } | null>(null);

  const stop = useCallback(() => {
    current.current?.stop();
    current.current = null;
    setLevel(0);
  }, []);

  const start = useCallback((stream: MediaStream) => {
    current.current?.stop();
    const ctx = new AudioContext();
    const analyser = ctx.createAnalyser();
    analyser.fftSize = 512;
    ctx.createMediaStreamSource(stream).connect(analyser);
    const buf = new Float32Array(analyser.fftSize);
    let raf = 0;
    let last = 0;
    const tick = (t: number) => {
      analyser.getFloatTimeDomainData(buf);
      let sum = 0;
      for (const v of buf) sum += v * v;
      if (t - last > 40) { setLevel(Math.min(1, Math.sqrt(sum / buf.length) * 6)); last = t; }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    const startedAt = Date.now();
    setElapsed(0);
    const timer = window.setInterval(() => setElapsed(Math.floor((Date.now() - startedAt) / 1000)), 500);
    current.current = { stop: () => { cancelAnimationFrame(raf); window.clearInterval(timer); void ctx.close(); } };
  }, []);

  useEffect(() => stop, [stop]);

  return { level, elapsed, start, stop };
}
