/**
 * The map's probe (design §5.9): the demo publishes its first map's on `window` with the lanes it
 * keeps itself; `__tesseraProbeOf` is the harness's own accessor, installed before any page
 * script, which falls back to the explorer's map's probe on a page that publishes none.
 */
interface Window {
  __tesseraProbeOf: () => Window['__tesseraProbe'] | null;
  __tesseraProbe?: {
    paints: number;
    at: number;
    marks: number;
    requests: number;
    encoding: string;
    view: {depth: number; status: string; stale: boolean; visible: number; matched: number; served: number; provisional: number};
    region: {depth: number; tiles: number; exact: boolean; visible: number; matched: number; held: number; status: string; ms: number | null} | null;
    timings: {
      slabMs: number;
      washMs: number;
      lutMs: number;
      outlinesMs: number;
      labelsMs: number;
      layersMs: number;
      lutWrites: number;
      outlines: number;
      labels: number;
      frame: {mean: number; p95: number; n: number};
      decodeMs: number[];
    };
    cluster: {
      layer: string | null;
      layersOn: string[];
      coverage: {current: number; stale: number};
      servedIds: string[];
      sample: {ordinal: number; resolvedId: string | null}[];
    };
    /** The three lanes' timings, kept by the demo (design §5.10's measurement); absent on any other page. */
    lanes?: {
      decode: {ms: number; workerMs: number | null; points: number; bytes: number; at: number}[];
      absorb: {split: number[]; store: number[]; remap: number[]; remapPoints: number[]; sliceMaxMs: number};
      region: Record<string, number> | null;
      coverage: Record<string, number> | null;
      longTasks: {ms: number; at: number}[];
    };
    instruments?: {depth: number; tiles: number; predictedMarks: number; limitedBy: string; bytes: number};
    [extra: string]: unknown;
  };
}
