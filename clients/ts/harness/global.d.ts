/** The map's probe, published on `window` by the demo (design §5.9) — what the harness reads. */
interface Window {
  __tesseraProbe?: {
    paints: number;
    at: number;
    marks: number;
    requests: number;
    encoding: string;
    view: {depth: number; status: string; stale: boolean; visible: number; matched: number; served: number; provisional: number};
    region: {depth: number; tiles: number; exact: boolean; visible: number; matched: number; held: number; status: string; ms: number | null} | null;
    timings: {slabMs: number; washMs: number; layersMs: number; frame: {mean: number; p95: number; n: number}; decodeMs: number[]};
    instruments?: {depth: number; tiles: number; predictedMarks: number; limitedBy: string; bytes: number};
    [extra: string]: unknown;
  };
}
