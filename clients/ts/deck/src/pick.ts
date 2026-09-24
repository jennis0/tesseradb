import {NO_ORDINAL, type ArtifactsProjection, type Band} from '@tesseradb/client';

/**
 * What {@link resolvePick} found under the pointer, by `kind`:
 *
 * - `mark`: an item's mark. `id` is the item's `tesseraId`. `worldXY` is the mark's position in
 *   world units, or the pointer's where the sublayer holds no positions, or `null` with neither.
 * - `artifact`: an artifact's name label. `id` is the artifact's `tesseraId`.
 * - `miss`: nothing under the pointer.
 * - `broken`: a hit on a sublayer with no id array, or at an `index` past its end. This is a defect
 *   in the layer, reported apart from a miss so the host can show it. `layer` is the sublayer's
 *   id, `hasIds` whether it has an id array and `idCount` that array's length.
 */
export type Picked =
  | {kind: 'mark'; id: bigint; worldXY: [number, number] | null}
  | {kind: 'artifact'; id: bigint}
  | {kind: 'miss'}
  | {kind: 'broken'; index: number; layer: string | null; hasIds: boolean; idCount: number};

/** The fields of deck.gl's `PickingInfo` that {@link resolvePick} reads. A `PickingInfo` can be passed as it is. */
export type PickInfo = {
  /** The index of the hit within the sublayer's data, or -1 for no hit. */
  index: number;
  /** The layer deck reports, read when `sourceLayer` is absent. */
  layer?: {id: string; props: object} | null;
  /** The sublayer that drew the hit. */
  sourceLayer?: {id: string; props: object} | null;
  /** The pointer's position in world units, used as a mark's `worldXY` when the sublayer holds no positions. */
  coordinate?: number[] | null;
};

/**
 * Resolves a deck.gl pick on a {@link TesseraLayer} to the item's mark or the artifact's name under
 * the pointer. Pass the pick info deck gives `onClick` or `onHover`. Only marks and artifact names
 * are pickable, so a pointer over an outline, a count or the wash away from any mark is a `miss`.
 */
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
