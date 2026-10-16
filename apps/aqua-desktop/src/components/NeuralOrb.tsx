import { useEffect, useRef } from 'react';
import type { MutableRefObject, MouseEventHandler } from 'react';

export type OrbPhase = 'idle' | 'listening' | 'thinking' | 'speaking';

interface Props {
  phase: OrbPhase;
  connected: boolean;
  /** Live voice level, 0..1: the microphone while listening, the reply audio while speaking. Written outside React. */
  levelRef: MutableRefObject<number>;
  size?: number;
  onMouseDown?: MouseEventHandler;
  onMouseMove?: MouseEventHandler;
  onMouseUp?: MouseEventHandler;
  onContextMenu?: MouseEventHandler;
}

type RGB = [number, number, number];

/** How each state looks and moves. The orb eases from one to the next, so a change reads as a transition, not a jump. */
interface Theme {
  core: RGB; // brightest colour, the centre and signal heads
  mid: RGB; // links and nodes
  deep: RGB; // the dome behind
  tempo: number; // how fast links form
  spin: number; // rotation speed, rad/s
  glow: number; // resting centre glow, 0..1
  signal: number; // share of links carrying a travelling pulse, 0..1
  dir: number; // pulse direction along a link: +1 inward, -1 outward, 0 either way
  drift: number; // how much nodes wander
  scan: number; // scanning ring, 0..1
}

const THEMES: Record<OrbPhase, Theme> = {
  // Idle: warm gold, slow and calm.
  idle: { core: [255, 226, 150], mid: [255, 176, 60], deep: [170, 85, 20], tempo: 1, spin: 0.16, glow: 0.22, signal: 0.12, dir: 0, drift: 0.02, scan: 0 },
  // Listening: cyan, everything drawn toward the centre.
  listening: { core: [200, 250, 255], mid: [40, 200, 255], deep: [10, 90, 175], tempo: 1.5, spin: 0.1, glow: 0.8, signal: 0.45, dir: 1, drift: 0.03, scan: 0 },
  // Processing: violet, fast and busy, with a scanning ring.
  thinking: { core: [245, 215, 255], mid: [176, 96, 255], deep: [85, 28, 170], tempo: 3.2, spin: 0.75, glow: 0.5, signal: 1, dir: 0, drift: 0.045, scan: 1 },
  // Speaking: green, a wave moving out from the centre with the voice.
  speaking: { core: [210, 255, 225], mid: [56, 232, 138], deep: [8, 125, 78], tempo: 1.7, spin: 0.2, glow: 0.45, signal: 0.5, dir: -1, drift: 0.03, scan: 0 },
};

interface Node { x: number; y: number; z: number; phase: number; inner: boolean }
interface Edge { a: number; b: number; offset: number; speed: number; sig: number }
interface Ring { born: number; life: number; from: number; to: number; alpha: number; width: number }

function rng(seed: number) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** A fixed network: a loose shell of nodes around a denser core, each joined to its nearest neighbours. */
function buildNetwork(): { nodes: Node[]; edges: Edge[] } {
  const rand = rng(11);
  const nodes: Node[] = [];
  const shell = 110;
  for (let i = 0; i < shell; i++) {
    const y = 1 - (i / (shell - 1)) * 2;
    const r = Math.sqrt(1 - y * y);
    const th = i * 2.399963;
    const k = 0.88 + rand() * 0.14;
    nodes.push({ x: Math.cos(th) * r * k, y: y * k, z: Math.sin(th) * r * k, phase: rand() * 6.28, inner: false });
  }
  for (let i = 0; i < 40; i++) {
    let x = 0, y = 0, z = 0;
    do { x = rand() * 2 - 1; y = rand() * 2 - 1; z = rand() * 2 - 1; } while (x * x + y * y + z * z > 1);
    nodes.push({ x: x * 0.52, y: y * 0.52, z: z * 0.52, phase: rand() * 6.28, inner: true });
  }
  const seen = new Set<number>();
  const edges: Edge[] = [];
  const len = (n: Node) => n.x * n.x + n.y * n.y + n.z * n.z;
  nodes.forEach((n, i) => {
    nodes
      .map((m, j) => ({ j, d: (n.x - m.x) ** 2 + (n.y - m.y) ** 2 + (n.z - m.z) ** 2 }))
      .filter((o) => o.j !== i)
      .sort((p, q) => p.d - q.d)
      .slice(0, n.inner ? 4 : 3)
      .forEach(({ j }) => {
        const key = Math.min(i, j) * 1000 + Math.max(i, j);
        if (seen.has(key)) return;
        seen.add(key);
        // Orient every link outer to inner, so "inward" and "outward" pulses are just a direction along it.
        const [a, b] = len(nodes[i]) >= len(nodes[j]) ? [i, j] : [j, i];
        edges.push({ a, b, offset: rand(), speed: 0.7 + rand() * 0.6, sig: rand() });
      });
  });
  return { nodes, edges };
}

/** Alpha levels links and nodes are grouped into, so a whole group is drawn with one call. */
const AB = 8;
const TAU = 6.2832;
const rgba = (c: RGB, a: number) => `rgba(${c[0] | 0}, ${c[1] | 0}, ${c[2] | 0}, ${Math.max(0, Math.min(1, a))})`;
const easeOut = (x: number) => 1 - (1 - x) * (1 - x) * (1 - x);

export function NeuralOrb({ phase, connected, levelRef, size = 300, onMouseDown, onMouseMove, onMouseUp, onContextMenu }: Props) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const live = useRef({ phase, connected });
  live.current = { phase, connected };

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext('2d');
    if (!canvas || !ctx) return;
    const dpr = Math.min(1.5, window.devicePixelRatio || 1);
    canvas.width = size * dpr;
    canvas.height = size * dpr;
    ctx.scale(dpr, dpr);

    const { nodes, edges } = buildNetwork();
    const c = size / 2;
    const baseR = size * 0.285;
    const maxRing = size * 0.49;

    // Everything below eases toward the current state's theme.
    const cur = { ...THEMES.idle, core: [...THEMES.idle.core] as RGB, mid: [...THEMES.idle.mid] as RGB, deep: [...THEMES.idle.deep] as RGB };
    let radius = baseR * 0.4; // grows in when the core connects
    let glow = 0.2;
    let pulse = 0; // smoothed voice level
    let spin = 0;
    let lastPhase: OrbPhase = live.current.phase;
    let lastRipple = 0;
    let scanAngle = 0;
    const rings: Ring[] = [];
    let last = performance.now();
    let raf = 0;
    // Scratch space reused every frame: nothing is allocated while the orb is drawn.
    const count = nodes.length;
    const px = new Float32Array(count), py = new Float32Array(count), pz = new Float32Array(count), pp = new Float32Array(count);
    const edgeBuckets: number[][] = Array.from({ length: AB * 2 }, () => []);
    const nodeBuckets: number[][] = Array.from({ length: AB * 2 }, () => []);
    const growDots: number[] = [];
    const sigBuckets: number[][] = Array.from({ length: 4 }, () => []);

    // The next frame is asked for only when it is due. Asking the browser for a callback on every screen refresh (120 a second
    // on a fast display) and returning at once from most of them woke this page 120 times a second for nothing, and kept the
    // graphics process producing frames at that rate too.
    let timer = 0;
    let stopped = false;
    const wait = () => {
      if (stopped) return;
      const { phase: ph, connected: up } = live.current;
      // Resting orb: its drift is slow, so 10 frames a second look the same and cost a third less than 15 (measured: about 1.8
      // points of a core). Working orb: 30 a second is smooth at this size. The old 60 a second cost about one and a half
      // processor cores, which the language model needs. Not on screen: look again in half a second.
      const interval = document.hidden ? 500 : ph === 'idle' ? (up ? 100 : 150) : 33;
      timer = window.setTimeout(() => { raf = requestAnimationFrame(frame); }, interval);
    };
    const frame = (now: number) => {
      wait();
      // Not on screen: draw nothing.
      if (document.hidden) {
        last = now;
        return;
      }
      const { phase: ph, connected: up } = live.current;
      const dt = Math.min(0.12, (now - last) / 1000);
      last = now;
      const t = now / 1000;
      const th = THEMES[ph];
      // The easing steps below were tuned per 60 Hz frame; this keeps them the same speed in real time at any rate.
      const kf = (c: number) => 1 - Math.pow(1 - c, dt * 60);

      const k = 1 - Math.exp(-dt * 4); // frame-rate independent easing
      for (let i = 0; i < 3; i++) {
        cur.core[i] += (th.core[i] - cur.core[i]) * k;
        cur.mid[i] += (th.mid[i] - cur.mid[i]) * k;
        cur.deep[i] += (th.deep[i] - cur.deep[i]) * k;
      }
      cur.tempo += (th.tempo - cur.tempo) * k;
      cur.spin += (th.spin - cur.spin) * k;
      cur.signal += (th.signal - cur.signal) * k;
      cur.dir += (th.dir - cur.dir) * k;
      cur.drift += (th.drift - cur.drift) * k;
      cur.scan += (th.scan - cur.scan) * k;

      // Fast to rise, slower to fall, so the orb follows the voice without jitter.
      const target = levelRef.current;
      pulse += (target - pulse) * kf(target > pulse ? 0.5 : 0.1);

      // A change of state sends a ring out from the centre, in the new colour.
      if (ph !== lastPhase) {
        rings.push({ born: t, life: 0.9, from: radius * 0.2, to: maxRing, alpha: 0.7, width: 2.5 });
        lastPhase = ph;
      }
      // Listening: ripples travel inward toward the centre. Speaking: ripples leave the surface with each swell.
      if (up && t - lastRipple > 0.22) {
        if (ph === 'listening' && pulse > 0.1) {
          rings.push({ born: t, life: 1.1, from: radius * 1.25, to: radius * 0.1, alpha: 0.25 + pulse * 0.45, width: 1.5 });
          lastRipple = t;
        } else if (ph === 'speaking' && pulse > 0.3) {
          rings.push({ born: t, life: 1.0, from: radius * 1.02, to: maxRing, alpha: 0.2 + pulse * 0.4, width: 1.5 });
          lastRipple = t;
        }
      }

      const swell = ph === 'speaking' ? pulse * 0.3 : ph === 'listening' ? pulse * 0.07 : 0;
      const wantR = !up ? baseR * 0.4 : baseR * (1 + swell + Math.sin(t * 1.3) * 0.018);
      radius += (wantR - radius) * kf(ph === 'speaking' ? 0.4 : 0.08);

      const wantGlow =
        ph === 'listening' ? th.glow + pulse * 0.3 : ph === 'speaking' ? th.glow + pulse * 0.45 : ph === 'thinking' ? th.glow + Math.sin(t * 6) * 0.15 : th.glow + Math.sin(t * 1.3) * 0.04;
      glow += (wantGlow - glow) * kf(0.14);

      spin += dt * cur.spin;
      scanAngle += dt * 3.2;

      ctx.clearRect(0, 0, size, size);
      ctx.globalCompositeOperation = 'lighter';

      // Dome, so the orb reads as a sphere.
      const dome = ctx.createRadialGradient(c, c, radius * 0.15, c, c, radius * 1.15);
      dome.addColorStop(0, rgba(cur.deep, 0.08));
      dome.addColorStop(0.75, rgba(cur.deep, 0.14));
      dome.addColorStop(1, rgba(cur.deep, 0));
      ctx.fillStyle = dome;
      ctx.beginPath();
      ctx.arc(c, c, radius * 1.15, 0, 6.2832);
      ctx.fill();

      // Project the nodes: spin, tilt, drift, and (while speaking) a radial wave that moves out from the centre.
      const cs = Math.cos(spin), sn = Math.sin(spin), tilt = 0.35, ct = Math.cos(tilt), st = Math.sin(tilt);
      const n = count;
      const waveAmp = ph === 'speaking' ? pulse * 0.2 : 0;
      for (let i = 0; i < n; i++) {
        const nd = nodes[i];
        const rr = Math.hypot(nd.x, nd.y, nd.z) || 1;
        const wave = 1 + waveAmp * Math.sin(t * 7 - rr * 5 + nd.phase * 0.3);
        const dx = Math.sin(t * 0.9 + nd.phase) * cur.drift;
        const dy = Math.cos(t * 0.8 + nd.phase * 1.3) * cur.drift;
        const dz = Math.sin(t * 0.7 + nd.phase * 0.7) * cur.drift;
        const x0 = (nd.x + dx) * wave, y0 = (nd.y + dy) * wave, z0 = (nd.z + dz) * wave;
        const x1 = x0 * cs + z0 * sn;
        const z1 = -x0 * sn + z0 * cs;
        const y2 = y0 * ct - z1 * st;
        const z2 = y0 * st + z1 * ct;
        const persp = 1 / (1 - z2 * 0.22);
        px[i] = c + x1 * radius * persp;
        py[i] = c + y2 * radius * persp;
        pz[i] = z2;
        pp[i] = persp;
      }

      // Links grow from one node to the other, hold, then fade. A pulse can ride a link in the state's direction.
      // Drawn in groups: one stroke per (alpha, width) group instead of one per link. Same picture, a fraction of the work.
      ctx.lineCap = 'round';
      for (const g of edgeBuckets) g.length = 0;
      growDots.length = 0;
      for (const g of sigBuckets) g.length = 0;
      const boost = 1 + (ph === 'speaking' || ph === 'listening' ? pulse * 0.8 : 0);
      for (const e of edges) {
        const cyc = (t * 0.22 * cur.tempo * e.speed + e.offset) % 1;
        let grow = 1, fade = 1;
        if (cyc < 0.3) grow = easeOut(cyc / 0.3);
        else if (cyc > 0.78) fade = 1 - (cyc - 0.78) / 0.22;
        const depth = ((pz[e.a] + pz[e.b]) / 2 + 1) / 2; // 0 far, 1 near
        const alpha = (0.1 + depth * 0.5) * fade * boost * (up ? 1 : 0.5);
        const x2 = px[e.a] + (px[e.b] - px[e.a]) * grow;
        const y2 = py[e.a] + (py[e.b] - py[e.a]) * grow;
        const ab = Math.min(AB - 1, Math.floor(Math.min(1, alpha) * AB));
        edgeBuckets[(depth > 0.5 ? AB : 0) + ab].push(px[e.a], py[e.a], x2, y2);
        if (grow < 1) growDots.push(x2, y2, 1 + depth * 1.2);
        if (up && e.sig < cur.signal) {
          let u = (t * 0.55 * cur.tempo * e.speed + e.offset * 3) % 1; // 0 at a (outer), 1 at b (inner)
          if (cur.dir < -0.3) u = 1 - u;
          const sa = (0.35 + depth * 0.55) * Math.sin(u * Math.PI);
          sigBuckets[Math.min(3, Math.floor(sa * 4))].push(px[e.a] + (px[e.b] - px[e.a]) * u, py[e.a] + (py[e.b] - py[e.a]) * u, 1.1 + depth * 1.3);
        }
      }
      for (let i = 0; i < edgeBuckets.length; i++) {
        const seg = edgeBuckets[i];
        if (!seg.length) continue;
        ctx.strokeStyle = rgba(cur.mid, ((i % AB) + 0.5) / AB);
        ctx.lineWidth = i >= AB ? 1.35 : 0.8;
        ctx.beginPath();
        for (let k = 0; k < seg.length; k += 4) {
          ctx.moveTo(seg[k], seg[k + 1]);
          ctx.lineTo(seg[k + 2], seg[k + 3]);
        }
        ctx.stroke();
      }
      if (growDots.length) {
        ctx.fillStyle = rgba(cur.core, 0.5);
        ctx.beginPath();
        for (let k = 0; k < growDots.length; k += 3) {
          ctx.moveTo(growDots[k] + growDots[k + 2], growDots[k + 1]);
          ctx.arc(growDots[k], growDots[k + 1], growDots[k + 2], 0, TAU);
        }
        ctx.fill();
      }
      for (let i = 0; i < sigBuckets.length; i++) {
        const d = sigBuckets[i];
        if (!d.length) continue;
        ctx.fillStyle = rgba(cur.core, (i + 0.5) / 4);
        ctx.beginPath();
        for (let k = 0; k < d.length; k += 3) {
          ctx.moveTo(d[k] + d[k + 2], d[k + 1]);
          ctx.arc(d[k], d[k + 1], d[k + 2], 0, TAU);
        }
        ctx.fill();
      }

      // Nodes: twinkle, with a soft halo; the inner ones brighten with the centre glow. Also drawn in groups.
      for (const g of nodeBuckets) g.length = 0;
      for (let i = 0; i < n; i++) {
        const nd = nodes[i];
        const depth = (pz[i] + 1) / 2;
        const tw = 0.6 + 0.4 * Math.sin(t * 2.2 + nd.phase);
        const lift = nd.inner ? 0.5 + glow * 0.9 : 0.45 + glow * 0.35;
        const a = Math.min(1, (0.25 + depth * 0.6) * tw * lift * (up ? 1 : 0.5));
        const r = (nd.inner ? 1.7 : 1.2 + depth * 1.5) * pp[i] * (1 + pulse * 0.25);
        nodeBuckets[(nd.inner ? AB : 0) + Math.min(AB - 1, Math.floor(a * AB))].push(px[i], py[i], r);
      }
      for (let pass = 0; pass < 2; pass++) {
        // pass 0: the soft halos; pass 1: the bright dots
        for (let i = 0; i < nodeBuckets.length; i++) {
          const d = nodeBuckets[i];
          if (!d.length) continue;
          const alpha = ((i % AB) + 0.5) / AB;
          ctx.fillStyle = rgba(i >= AB ? cur.core : cur.mid, pass === 0 ? alpha * 0.22 : alpha);
          const grow = pass === 0 ? 3.2 : 1;
          ctx.beginPath();
          for (let k = 0; k < d.length; k += 3) {
            const r = d[k + 2] * grow;
            ctx.moveTo(d[k] + r, d[k + 1]);
            ctx.arc(d[k], d[k + 1], r, 0, TAU);
          }
          ctx.fill();
        }
      }

      // Centre glow.
      const gr = radius * (0.35 + glow * 0.6);
      const core = ctx.createRadialGradient(c, c, 0, c, c, gr);
      core.addColorStop(0, rgba(cur.core, 0.15 + glow * 0.75));
      core.addColorStop(0.35, rgba(cur.mid, 0.08 + glow * 0.4));
      core.addColorStop(1, rgba(cur.mid, 0));
      ctx.fillStyle = core;
      ctx.beginPath();
      ctx.arc(c, c, gr, 0, 6.2832);
      ctx.fill();

      // Scanning ring while processing: two arcs chasing each other around the orb.
      if (cur.scan > 0.02) {
        ctx.lineWidth = 2;
        ctx.strokeStyle = rgba(cur.core, 0.7 * cur.scan);
        for (let s = 0; s < 2; s++) {
          const a0 = scanAngle + s * Math.PI;
          ctx.beginPath();
          ctx.arc(c, c, radius * 1.16, a0, a0 + 1.0);
          ctx.stroke();
        }
        ctx.strokeStyle = rgba(cur.mid, 0.25 * cur.scan);
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.arc(c, c, radius * 1.16, 0, 6.2832);
        ctx.stroke();
      }

      // Rings (state-change bursts and voice ripples).
      for (let i = rings.length - 1; i >= 0; i--) {
        const r = rings[i];
        const age = (t - r.born) / r.life;
        if (age >= 1) { rings.splice(i, 1); continue; }
        const rad = r.from + (r.to - r.from) * easeOut(age);
        ctx.strokeStyle = rgba(cur.core, r.alpha * (1 - age));
        ctx.lineWidth = r.width * (1 - age * 0.5);
        ctx.beginPath();
        ctx.arc(c, c, Math.max(1, rad), 0, 6.2832);
        ctx.stroke();
      }
    };
    raf = requestAnimationFrame(frame);
    // Coming back on screen: draw straight away instead of waiting out the half second.
    const onVisible = () => {
      if (stopped || document.hidden) return;
      window.clearTimeout(timer);
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(frame);
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      stopped = true;
      window.clearTimeout(timer);
      cancelAnimationFrame(raf);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [size, levelRef]);

  return (
    <canvas
      ref={canvasRef}
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={onMouseUp}
      onContextMenu={onContextMenu}
      style={{ width: size, height: size, cursor: phase === 'idle' ? 'pointer' : 'default', animation: 'orbAppear 0.7s cubic-bezier(0.175, 0.885, 0.32, 1.275)' }}
    />
  );
}
