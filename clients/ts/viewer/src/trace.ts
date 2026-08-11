/**
 * A session recorder for the demo: what the user did, what the client did about it, and how long
 * each frame took.
 *
 * **Why this exists.** The browser-free harness (`probes/2026-08-09-client-pipeline/`) attributes
 * client CPU well and cannot see the three things a user actually feels — GPU upload, frame
 * scheduling, and the relationship between an input and the paint that answers it. Every diagnosis
 * made without them this far has needed at least one correction. This closes that loop: the reader
 * gets a timeline in which an input, the work it caused, and the frame that resulted are the same
 * three records.
 *
 * **Off unless asked for.** `?trace=1`. Disabled, every method returns on a boolean and no observer,
 * no animation frame and no listener is installed — an instrument that perturbs what it measures is
 * worse than none, and this one runs inside the frame loop it is timing.
 *
 * **Bounded.** A ring of {@link CAPACITY} events, oldest dropped. A long session is meant to end in
 * a download, not an out-of-memory.
 *
 * The marker key (`m`) is the point of the whole thing: press it when something feels wrong, and the
 * trace carries the reader's own judgement rather than leaving it to be guessed from the numbers.
 */

/** ~10 minutes of 60 Hz frames plus everything else that happens alongside them. */
const CAPACITY = 60_000;

/**
 * One record. Deliberately flat and numeric.
 *
 * Objects rather than a typed ring because the volume is bounded and readability beats the
 * allocation: 60k small objects is a few MB, against a frame budget of 16 ms that a `push` does not
 * threaten. A packed encoding would be faster to write and much harder to trust.
 */
export type TraceEvent = {
  /** Milliseconds since the trace started. */
  t: number;
  kind: string;
  /** How long the thing took, where it is a duration. */
  ms?: number;
  /** The thing's natural count — marks, bands, bytes, whichever the kind names. */
  n?: number;
  [field: string]: number | string | undefined;
};

export type TraceHeader = {
  startedAt: string;
  href: string;
  userAgent: string;
  devicePixelRatio: number;
  viewport: {width: number; height: number};
  /** The real GPU, via `WEBGL_debug_renderer_info` — software rasterisation invalidates timings. */
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
   * The recent frames, one entry each — the two targets are computed from these alone.
   *
   * A bounded recent history rather than session totals, because the targets exempt the initial
   * load: slow-while-loading is accepted, slow-while-exploring is the thing being fixed. A trailing
   * window scores what the user is doing *now*, so the bar reads red exactly when the map feels
   * rough and recovers when it stops.
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

  /** Record one event. The hot path: a timestamp, an object and a ring write. */
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

  /**
   * Time `fn` and record it, returning what it returned.
   *
   * Returns `fn()` uninstrumented when disabled — no closure, no clock read — so a call site can
   * wrap a hot function without conditioning on the flag itself.
   */
  phase<T>(kind: string, fn: () => T, fields?: Record<string, number | string | undefined>): T {
    if (!this.enabled) return fn();
    const start = performance.now();
    const out = fn();
    this.event(kind, {...fields, ms: performance.now() - start});
    return out;
  }

  /**
   * Count one animation frame. Every frame, not only the slow ones — the average is over all of
   * them, and recording only the misses would make the score look worse than the session felt.
   */
  noteFrame(gap: number): void {
    if (!this.enabled) return;
    this.recent.push({t: performance.now(), gap});
    // Trimmed in bulk, amortised: ~68 s of 60 Hz frames, comfortably past the scoring window.
    if (this.recent.length > 8192) this.recent.splice(0, 4096);
  }

  /**
   * The score against the two targets over the last `windowMs`: average fps and p95 frame time.
   *
   * Exact over the window — every frame in it is recorded, so the quantile is a real quantile
   * rather than an estimate from the misses.
   */
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

  /** Chronological, oldest first — the ring unrolled. */
  private ordered(): TraceEvent[] {
    if (this.events.length < CAPACITY) return this.events;
    return [...this.events.slice(this.cursor), ...this.events.slice(0, this.cursor)];
  }

  toJSON(): {header: TraceHeader | null; events: TraceEvent[]} {
    return {header: this.header, events: this.ordered()};
  }

  /** Hand the reader a file. A download rather than a console dump: 60k records is not readable. */
  download(): void {
    const blob = new Blob([JSON.stringify(this.toJSON())], {type: 'application/json'});
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `tessera-trace-${new Date().toISOString().replace(/[:.]/g, '-')}.json`;
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
    __tesseraTrace?: Trace;
  }
}
if (trace.enabled && typeof window !== 'undefined') window.__tesseraTrace = trace;

/**
 * Frame timing, long tasks, and input — the three the harness cannot see.
 *
 * **Frame gaps are recorded, not frame *work*.** A gap is what a user perceives; attributing it
 * needs the phase records that sit between two gaps, which is exactly what the timeline gives.
 * Only gaps past {@link SLOW_FRAME_MS} are recorded, so a smooth stretch costs one number rather
 * than one record per frame — the interesting frames are the ones that missed.
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

  // Long tasks say *that* the main thread blocked; the phase records around them say what in. The
  // pair is the whole diagnosis, and neither half is enough alone.
  try {
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        trace.event('longtask', {ms: entry.duration, name: entry.name});
      }
    }).observe({entryTypes: ['longtask']});
  } catch {
    // Not in every browser, and its absence is not a reason to record nothing else.
  }

  // Input, so a gap can be read against what the user was doing when it happened. Pointer *moves*
  // are counted rather than recorded: at 120 Hz they would be most of the file and none of the
  // signal, and the drag is bounded by the down and up either side of it.
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

/** The GPU's own name, so a trace taken under software rasterisation can be discarded on sight. */
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

/**
 * The recording bar.
 *
 * Fixed and out of the layout, because the panels are themselves a thing being measured and a
 * recorder that reflows them is recording its own effect.
 */
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
    // The two targets, scored live over the last ten seconds: average fps above 45, p95 frame
    // time under 100 ms. A trailing window because the targets exempt the initial load — the bar
    // is meant to read red exactly while the map feels rough, and to recover when it stops.
    const fpsOk = stats.fps > 45;
    const p95Ok = stats.p95 < 100;
    const paint = (ok: boolean) => (ok ? '#7ad694' : '#ed7689');
    label.innerHTML =
      `<b style="color:${paint(fpsOk)}">${stats.fps.toFixed(0)} fps</b> · ` +
      `<b style="color:${paint(p95Ok)}">p95 ${stats.p95.toFixed(0)} ms</b> <span style="opacity:.6">(10 s)</span> · ` +
      `${trace.count} events, ${trace.markerCount} marks`;
  };
  render();
  // Repainted on a timer rather than per event: the bar is not worth a DOM write per frame, and a
  // recorder whose own UI shows up in the trace is measuring itself.
  setInterval(render, 500);
}
