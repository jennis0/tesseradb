/**
 * What the smoke scripts read off the page, for the typecheck that covers them.
 *
 * The definition is `@tesseradb/components`' `MapProbe` (design §5.9), which the demo publishes on
 * `window` with the lanes it keeps itself; this declares the fields these six scripts actually
 * read and no more, because a script that reads a field it has not declared is exactly the mistake
 * the check exists to catch. `clients/ts/harness/global.d.ts` declares the harness's own view of
 * the same object — a separate `tsc` program, hence a separate file.
 */
interface Window {
  __tesseraProbe?: {
    paints: number;
    marks: number;
    requests: number;
    view: {depth: number; status: string; visible: number; matched: number; served: number; provisional: number};
    timings: {outlines: number; labels: number};
    cluster: {layersOn: string[]; servedIds: string[]};
    /** The demo's instrument numbers, which the §4 surface omits and the demo publishes. */
    instruments?: {depth: number; tiles: number; predictedMarks: number; limitedBy: string; bytes: number};
  };
}
