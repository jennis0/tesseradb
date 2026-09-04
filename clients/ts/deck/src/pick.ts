import {NO_ORDINAL, type ArtifactsProjection, type Band} from '@tesseradb/client';

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
  const props = (layer?.props ?? {}) as {tesseraIds?: BigUint64Array; tesseraPositions?: Float32Array; artifactIds?: bigint[]};
  // An artifact's label answered — a different kind of thing, on its own route. `artifactIds` is a
  // **row-to-artifact map**, not an index into the served set: a wrapped name is several text rows
  // of one label, and every one of them carries the artifact's own identifier.
  //
  // **A contour does not come through here.** The outline layer draws the hovered and the opened
  // artifact and is not pickable; what the pointer is over is resolved against the frontier's
  // served shapes in JS (`hoverAt`), for a click as for a hover.
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
  // **The mark's own position, not the pointer's.** `info.coordinate` is where the cursor was,
  // which is up to a pick radius from the mark it hit — a fixed *world* offset, so a marker drawn
  // there separates from its point by that offset times every further zoom. The positions ride
  // with the ids on the same sublayer and are the buffer deck drew from, so index `i` is exact.
  const pos = props.tesseraPositions;
  const worldXY: [number, number] | null =
    pos && info.index * 2 + 1 < pos.length
      ? [pos[info.index * 2]!, pos[info.index * 2 + 1]!]
      : info.coordinate
        ? [info.coordinate[0]!, info.coordinate[1]!]
        : null;
  return {kind: 'mark', id: ids[info.index]!, worldXY};
}

/**
 * The served artifact a mark is a member of, for a hover: the mark's ordinal for the first layer
 * on, resolved up the session table to the served set at `level` (the deepest served when
 * undefined). Null where the band carries no column for the layer, the mark is under no
 * artifact, or the walk fails — exactly the marks that draw neutral under cluster colour.
 */
export function artifactOfMark(band: Band, i: number, artifacts: ArtifactsProjection, level?: number): bigint | null {
  const layer = artifacts.layers[0];
  if (!layer) return null;
  const column = band.membership[layer];
  if (!column) return null;
  const ordinal = column.ordinals[i] ?? NO_ORDINAL;
  const served = artifacts.table.resolve(ordinal, artifacts.servedOrdinals, level);
  if (served === NO_ORDINAL) return null;
  return artifacts.table.entry(served)?.tesseraId ?? null;
}
