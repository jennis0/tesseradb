/**
 * What a deck.gl pick on a `TesseraLayer` resolved to.
 *
 * A miss and a broken pick are different failures and the host shows which: `index < 0` is deck
 * reporting nothing under the cursor, the ordinary case; a hit whose sublayer carries no identity
 * array, or an index past the end of one, is a defect in the layer and says so rather than
 * reading as a miss for a whole session (found that way once in the instrument).
 */
export type Picked =
  | {kind: 'mark'; id: bigint; worldXY: [number, number] | null}
  | {kind: 'artifact'; id: bigint}
  | {kind: 'miss'}
  | {kind: 'broken'; index: number; layer: string | null; hasIds: boolean; idCount: number};

/** The slice of deck's `PickingInfo` this needs — typed narrowly so a test can build one. */
export type PickInfo = {
  index: number;
  layer?: {id: string; props: object} | null;
  sourceLayer?: {id: string; props: object} | null;
  coordinate?: number[] | null;
};

export function resolvePick(info: PickInfo): Picked {
  if (info.index < 0) return {kind: 'miss'};
  const layer = info.sourceLayer ?? info.layer;
  const props = (layer?.props ?? {}) as {tesseraIds?: BigUint64Array; artifactIds?: bigint[]};
  // An artifact marker answered — a different kind of thing, on its own route.
  if (props.artifactIds) {
    const id = props.artifactIds[info.index];
    if (id === undefined) {
      return {kind: 'broken', index: info.index, layer: layer?.id ?? null, hasIds: true, idCount: props.artifactIds.length};
    }
    return {kind: 'artifact', id};
  }
  const ids = props.tesseraIds;
  if (!ids || info.index >= ids.length) {
    return {kind: 'broken', index: info.index, layer: layer?.id ?? null, hasIds: ids !== undefined, idCount: ids?.length ?? 0};
  }
  const worldXY = info.coordinate ? ([info.coordinate[0]!, info.coordinate[1]!] as [number, number]) : null;
  return {kind: 'mark', id: ids[info.index]!, worldXY};
}
