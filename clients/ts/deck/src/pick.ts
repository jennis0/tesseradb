import {NO_ORDINAL, type ArtifactsProjection, type Band} from '@tesseradb/client';

/**
 * What a deck.gl pick on a `TesseraLayer` resolved to. `miss` is deck reporting nothing under the
 * cursor. `broken` is a hit on a sublayer with no identity array, or an index past its end: a
 * defect in the layer, reported apart from a miss so the host can show it.
 */
export type Picked =
  | {kind: 'mark'; id: bigint; worldXY: [number, number] | null}
  | {kind: 'artifact'; id: bigint}
  | {kind: 'miss'}
  | {kind: 'broken'; index: number; layer: string | null; hasIds: boolean; idCount: number};

/** The part of deck's `PickingInfo` a pick reads, typed narrowly so a test can build one. */
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
  // A label hit. `artifactIds` maps each text row to its artifact: a wrapped name is several rows
  // of one label. Contours are not pickable; `hoverAt` resolves them in JS.
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
  // The mark's own position from the buffer deck drew. `info.coordinate` is the cursor, up to a
  // pick radius away, and a marker placed there drifts from its point as the camera zooms in.
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
 * The served artifact mark `i` belongs to, for a hover: its ordinal on the first layer on,
 * resolved up the session table to the served set at `level` (the deepest served when undefined).
 * Null for the marks that draw neutral under cluster colour: no column for the layer, no
 * artifact, or no served ancestor.
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
