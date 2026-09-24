/**
 * What the smoke scripts read off the page, for the typecheck that covers them. The object is
 * `@tesseradb/components`' `MapProbe`, which the viewer publishes on `window` with its timing
 * lanes. Only the fields the scripts read are declared, so reading an undeclared one fails the
 * typecheck. `clients/ts/harness/global.d.ts` declares the harness's view of the same object for
 * a separate `tsc` program.
 */
interface Window {
  __tesseraProbe?: {
    paints: number;
    marks: number;
    requests: number;
    view: {depth: number; status: string; visible: number; matched: number; served: number; provisional: number};
    /**
     * The outline layer draws only the hovered and opened artifacts, so both counts are 0 at rest.
     * `outlines` counts parts and `outlinesDrawn` the artifacts they belong to.
     */
    timings: {outlines: number; outlinesDrawn: number; labels: number};
    cluster: {layersOn: string[]; servedIds: string[]};
    /** The selected region: the server's verdict, its count and whether it is exact. */
    region: {verdict: string; exact: boolean; visible: number | null; matched: number; held: number; status: string; ms: number | null} | null;
    /** The viewer's instrument numbers, which the store reports on its `instruments` channel. */
    instruments?: {depth: number; tiles: number; predictedMarks: number; source: string; limitedBy: string; bytes: number};
  };
}
