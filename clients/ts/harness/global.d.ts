/**
 * The map's probe. The viewer publishes its first map's on `window` with its timing lanes.
 * `__tesseraProbeOf` is the harness's accessor, installed before any page script, which falls
 * back to the explorer's map's probe on a page that publishes none.
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
      /** How many of the sampled ordinals the lookup texture colours. */
      coloured: number;
    };
    /** Decode, absorb and region timings, kept by the viewer; absent on any other page. */
    lanes?: {
      decode: {ms: number; workerMs: number | null; points: number; bytes: number; at: number}[];
      absorb: {split: number[]; store: number[]; remap: number[]; remapPoints: number[]; sliceMaxMs: number};
      /** `region` and `coverage` carry numeric fields only. */
      region: Record<string, number> | null;
      coverage: Record<string, number> | null;
      longTasks: {ms: number; at: number}[];
    };
    instruments?: {depth: number; tiles: number; predictedMarks: number; source: string; limitedBy: string; bytes: number};
    [extra: string]: unknown;
  };
}
