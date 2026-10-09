/**
 * A session recorder for the viewer: inputs, the work they caused and the frames that resulted, on
 * one timeline, which a browser-free harness cannot show (GPU upload, frame scheduling).
 *
 * Enabled by `?trace=1`. Disabled, every method returns on a boolean and no observer, animation
 * frame or listener is installed. Events go in a ring of {@link CAPACITY}, oldest dropped, and the
 * session ends in a download. Pressing `m` records a marker where something felt wrong.
 */

/** About ten minutes of 60 Hz frames and the events alongside them. */
const CAPACITY = 60_000;

/** One record, flat and mostly numeric. At the ring's capacity the objects take a few MB. */
export type TraceEvent = {
  /** Milliseconds since the trace started. */
  t: number;
  kind: string;
  /** How long the thing took, where it is a duration. */
  ms?: number;
  /** The count the kind names: marks, bands or bytes. */
  n?: number;
  [field: string]: number | string | undefined;
};

export type TraceHeader = {
  startedAt: string;
  href: string;
  userAgent: string;
  devicePixelRatio: number;
  viewport: {width: number; height: number};
  /** The GPU, from `WEBGL_debug_renderer_info`; timings under software rasterisation are void. */
  renderer: string;
};

class Trace {
  readonly enabled: boolean;
  private events: TraceEvent[] = [];
  private cursor = 0;
  private origin = 0;
  private header: TraceHeader | null = null;
  private markers = 0;
  private onChange: (() => void) | null = null;
  /**
   * The recent frames, one entry each, which the frame targets are scored from. A trailing window
   * rather than session totals, so the initial load is not counted.
   */
  private recent: {t: number; gap: number}[] = [];

  constructor(enabled: boolean) {
    this.enabled = enabled;
    if (enabled) this.origin = performance.now();
  }

  get count(): number {
    return this.events.length;
  }
  get markerCount(): number {
    return this.markers;
  }

  /** Record one event. */
  event(kind: string, fields?: Record<string, number | string | undefined>): void {
    if (!this.enabled) return;
    const record: TraceEvent = {t: performance.now() - this.origin, kind, ...fields};
    if (this.events.length < CAPACITY) this.events.push(record);
    else {
      this.events[this.cursor] = record;
      this.cursor = (this.cursor + 1) % CAPACITY;
    }
    this.onChange?.();
  }

  /** Time `fn` and record it, returning its result. Disabled, it calls `fn` and reads no clock. */
  phase<T>(kind: string, fn: () => T, fields?: Record<string, number | string | undefined>): T {
    if (!this.enabled) return fn();
    const start = performance.now();
    const out = fn();
    this.event(kind, {...fields, ms: performance.now() - start});
    return out;
  }

  /** Count one animation frame. Every frame is counted, so the average and quantile are exact. */
  noteFrame(gap: number): void {
    if (!this.enabled) return;
    this.recent.push({t: performance.now(), gap});
    // Trimmed in bulk; 8,192 frames at 60 Hz is about 68 s, past the scoring window.
    if (this.recent.length > 8192) this.recent.splice(0, 4096);
  }

  /** Average fps and p95 frame time over the last `windowMs`. */
  frameStats(windowMs = 10_000): {fps: number; p95: number} | null {
    const since = performance.now() - windowMs;
    let start = this.recent.length;
    while (start > 0 && this.recent[start - 1]!.t >= since) start--;
    const inWindow = this.recent.length - start;
    if (inWindow < 30) return null;
    const span = performance.now() - this.recent[start]!.t;
    const gaps = this.recent.slice(start).map((f) => f.gap).sort((a, b) => a - b);
    return {
      fps: (inWindow / Math.max(1, span)) * 1000,
      p95: gaps[Math.min(gaps.length - 1, Math.ceil(gaps.length * 0.95) - 1)]!
    };
  }

  /** Something the reader judged, at the moment they judged it. */
  mark(label: string): void {
    if (!this.enabled) return;
    this.markers++;
    this.event('marker', {label});
  }

  describe(header: TraceHeader): void {
    if (!this.enabled) return;
    this.header = header;
  }

  notify(onChange: () => void): void {
    this.onChange = onChange;
  }

  /** The ring unrolled, oldest first. */
  private ordered(): TraceEvent[] {
    if (this.events.length < CAPACITY) return this.events;
    return [...this.events.slice(this.cursor), ...this.events.slice(0, this.cursor)];
  }

  toJSON(): {header: TraceHeader | null; events: TraceEvent[]} {
    return {header: this.header, events: this.ordered()};
  }

  /** Download the trace as a JSON file. */
  download(): void {
    const blob = new Blob([JSON.stringify(this.toJSON())], {type: 'application/json'});
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `mosaica-trace-${new Date().toISOString().replace(/[:.]/g, '-')}.json`;
    a.click();
    URL.revokeObjectURL(url);
  }

  clear(): void {
    this.events = [];
    this.cursor = 0;
    this.markers = 0;
    this.recent = [];
    this.origin = performance.now();
    this.onChange?.();
  }
}

export const trace = new Trace(
  typeof location !== 'undefined' && new URLSearchParams(location.search).get('trace') === '1'
);

declare global {
  interface Window {
    __mosaicaTrace?: Trace;
  }
}
if (trace.enabled && typeof window !== 'undefined') window.__mosaicaTrace = trace;

/**
 * Frame gaps longer than this are recorded as events; every frame is still counted for the score.
 * The phase records between two gaps say what the time went on.
 */
const SLOW_FRAME_MS = 20;

export function installTrace(canvas: HTMLElement): void {
  if (!trace.enabled) return;

  trace.describe({
    startedAt: new Date().toISOString(),
    href: location.href,
    userAgent: navigator.userAgent,
    devicePixelRatio: devicePixelRatio,
    viewport: {width: canvas.clientWidth, height: canvas.clientHeight},
    renderer: rendererName()
  });

  let last = performance.now();
  let smooth = 0;
  const tick = () => {
    const now = performance.now();
    const gap = now - last;
    last = now;
    trace.noteFrame(gap);
    if (gap > SLOW_FRAME_MS) {
      trace.event('frame', {ms: gap, n: smooth});
      smooth = 0;
    } else {
      smooth++;
    }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);

  // Long tasks show that the main thread blocked; the phase records around them show on what.
  try {
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        trace.event('longtask', {ms: entry.duration, name: entry.name});
      }
    }).observe({entryTypes: ['longtask']});
  } catch {
    // Not in every browser.
  }

  // Input, so a gap can be read against what the user was doing. Pointer moves are counted, not
  // recorded, since at 120 Hz they would fill the ring.
  let moves = 0;
  canvas.addEventListener('pointerdown', (e) => {
    moves = 0;
    trace.event('pointerdown', {x: Math.round(e.clientX), y: Math.round(e.clientY)});
  });
  canvas.addEventListener('pointermove', () => {
    moves++;
  });
  canvas.addEventListener('pointerup', (e) => {
    trace.event('pointerup', {x: Math.round(e.clientX), y: Math.round(e.clientY), n: moves});
  });
  canvas.addEventListener('wheel', (e) => trace.event('wheel', {n: e.deltaY}), {passive: true});

  addEventListener('keydown', (e) => {
    // Not while typing into a panel control.
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;
    if (e.key === 'm') trace.mark('felt wrong');
  });
}

/** The GPU's name, so a trace taken under software rasterisation can be discarded. */
function rendererName(): string {
  try {
    const gl = document.createElement('canvas').getContext('webgl2');
    if (!gl) return 'no webgl2';
    const ext = gl.getExtension('WEBGL_debug_renderer_info');
    return ext ? String(gl.getParameter(ext.UNMASKED_RENDERER_WEBGL)) : gl.getParameter(gl.RENDERER);
  } catch {
    return 'unknown';
  }
}

/** The recording bar, fixed and out of the layout so it does not reflow the panels it measures. */
export function installTraceBar(): void {
  if (!trace.enabled) return;
  const bar = document.createElement('div');
  bar.style.cssText =
    'position:fixed;bottom:8px;left:8px;z-index:9999;background:#1b1d22;color:#e6e6e6;' +
    'border:1px solid #3a3f47;border-radius:6px;padding:6px 10px;font:12px system-ui;' +
    'display:flex;gap:10px;align-items:center';
  const label = document.createElement('span');
  const save = document.createElement('button');
  save.textContent = 'download trace';
  save.onclick = () => trace.download();
  const reset = document.createElement('button');
  reset.textContent = 'reset';
  reset.onclick = () => trace.clear();
  const marker = document.createElement('button');
  marker.textContent = 'mark (m)';
  marker.onclick = () => trace.mark('felt wrong');
  bar.append(label, marker, save, reset);
  document.body.append(bar);

  const render = () => {
    const stats = trace.frameStats();
    if (!stats) {
      label.innerHTML = `● ${trace.count} events, ${trace.markerCount} marks`;
      return;
    }
    // The targets over the last ten seconds: average fps above 45, p95 frame time under 100 ms.
    const fpsOk = stats.fps > 45;
    const p95Ok = stats.p95 < 100;
    const paint = (ok: boolean) => (ok ? '#7ad694' : '#ed7689');
    label.innerHTML =
      `<b style="color:${paint(fpsOk)}">${stats.fps.toFixed(0)} fps</b> · ` +
      `<b style="color:${paint(p95Ok)}">p95 ${stats.p95.toFixed(0)} ms</b> <span style="opacity:.6">(10 s)</span> · ` +
      `${trace.count} events, ${trace.markerCount} marks`;
  };
  render();
  // Repainted on a timer, so the bar's own DOM writes stay out of the frames it measures.
  setInterval(render, 500);
}
