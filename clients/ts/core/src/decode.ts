import {tableFromIPC, Type, type DataType, type Table, type Vector} from 'apache-arrow';
import {CELLS_PER_WORLD_UNIT} from './coords.js';
import {splitFramedStreams} from './frame.js';
import type {Artifact, MembershipColumn, ScalarColumn, ScalarValues, SubCell, TileCounts, ViewportResult} from './types.js';

function u64Column(table: Table, name: string): BigUint64Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as BigUint64Array;
}

/**
 * One declared scalar column: the declared type and the buffer Arrow already holds.
 *
 * Numeric columns use `toArray()`, which returns Arrow's typed array; a spread would box every
 * element. `bool` and `utf8` have no typed form and are materialised. A type the manifest cannot
 * declare throws, since skipping it would drop the column with no error. The arms mirror
 * `mosaica_wire::payload::ScalarColumn`, and the two change together. Nulls are read into
 * `present`; `toArray()` gives a null number's slot as `0`, and that value means nothing.
 */
function scalarColumn(name: string, vector: Vector<DataType>): ScalarColumn {
  const column: ScalarColumn = scalarValues(name, vector);
  if (vector.nullCount > 0) {
    const present = new Uint8Array(vector.length);
    for (let i = 0; i < vector.length; i++) present[i] = vector.isValid(i) ? 1 : 0;
    column.present = present;
  }
  return column;
}

function scalarValues(name: string, vector: Vector<DataType>): ScalarValues {
  const type = vector.type;
  switch (type.typeId) {
    case Type.Bool:
      return {arrowType: 'bool', values: Array.from(vector, (v: boolean | null) => v === true)};
    case Type.Utf8:
      return {arrowType: 'utf8', values: Array.from(vector, (v: string | null) => v ?? '')};
    case Type.Timestamp:
      return {arrowType: 'timestamp_us', values: vector.toArray() as BigInt64Array};
    case Type.Int: {
      // `Int` covers all eight widths; the bit width and signedness are on the type.
      const {bitWidth, isSigned} = type as unknown as {bitWidth: number; isSigned: boolean};
      const key = `${isSigned ? 'i' : 'u'}${bitWidth}`;
      switch (key) {
        case 'u8':
          return {arrowType: 'u8', values: vector.toArray() as Uint8Array};
        case 'u16':
          return {arrowType: 'u16', values: vector.toArray() as Uint16Array};
        case 'u32':
          return {arrowType: 'u32', values: vector.toArray() as Uint32Array};
        case 'u64':
          return {arrowType: 'u64', values: vector.toArray() as BigUint64Array};
        case 'i8':
          return {arrowType: 'i8', values: vector.toArray() as Int8Array};
        case 'i16':
          return {arrowType: 'i16', values: vector.toArray() as Int16Array};
        case 'i32':
          return {arrowType: 'i32', values: vector.toArray() as Int32Array};
        case 'i64':
          return {arrowType: 'i64', values: vector.toArray() as BigInt64Array};
      }
      break;
    }
    case Type.Float: {
      const {precision} = type as unknown as {precision: number};
      // Arrow `Precision`: 0 = HALF, 1 = SINGLE, 2 = DOUBLE. Only the latter two are declarable.
      if (precision === 1) return {arrowType: 'f32', values: vector.toArray() as Float32Array};
      if (precision === 2) return {arrowType: 'f64', values: vector.toArray() as Float64Array};
      break;
    }
  }
  throw new Error(`viewport column "${name}" has a type this decoder cannot read: ${type}`);
}

/**
 * Concatenates one declared column's per-frame pieces into one {@link ScalarColumn}. Every frame
 * has the same schema; a mismatch is refused rather than mixing two columns' values.
 */
function concatScalarColumns(pieces: ScalarColumn[], total: number): ScalarColumn {
  const first = pieces[0]!;
  if (pieces.length === 1) return first;
  for (const piece of pieces) {
    if (piece.arrowType !== first.arrowType) {
      throw new Error(
        `frames disagree on a column's type: ${piece.arrowType} vs ${first.arrowType}`
      );
    }
  }
  const column: ScalarColumn = concatScalarValues(pieces, total);
  if (pieces.some((p) => p.present)) {
    const present = new Uint8Array(total).fill(1);
    let offset = 0;
    for (const piece of pieces) {
      if (piece.present) present.set(piece.present, offset);
      offset += piece.values.length;
    }
    column.present = present;
  }
  return column;
}

function concatScalarValues(pieces: ScalarColumn[], total: number): ScalarValues {
  const first = pieces[0]!;
  if (first.arrowType === 'bool' || first.arrowType === 'utf8') {
    return {
      arrowType: first.arrowType,
      values: pieces.flatMap((p) => p.values as (boolean | string)[])
    } as ScalarValues;
  }
  // The switch names each typed-array constructor for the type checker. It mirrors `scalarColumn`'s
  // arms, and the two change together.
  const fill = <A extends {set(a: A, o: number): void; length: number}>(out: A): A => {
    let offset = 0;
    for (const piece of pieces) {
      // The same `arrowType` means the same typed-array class, which the checker cannot see.
      const values = piece.values as unknown as A;
      out.set(values, offset);
      offset += values.length;
    }
    return out;
  };
  switch (first.arrowType) {
    case 'u8':
      return {arrowType: 'u8', values: fill(new Uint8Array(total))};
    case 'u16':
      return {arrowType: 'u16', values: fill(new Uint16Array(total))};
    case 'u32':
      return {arrowType: 'u32', values: fill(new Uint32Array(total))};
    case 'u64':
      return {arrowType: 'u64', values: fill(new BigUint64Array(total))};
    case 'i8':
      return {arrowType: 'i8', values: fill(new Int8Array(total))};
    case 'i16':
      return {arrowType: 'i16', values: fill(new Int16Array(total))};
    case 'i32':
      return {arrowType: 'i32', values: fill(new Int32Array(total))};
    case 'i64':
      return {arrowType: 'i64', values: fill(new BigInt64Array(total))};
    case 'timestamp_us':
      return {arrowType: 'timestamp_us', values: fill(new BigInt64Array(total))};
    case 'f32':
      return {arrowType: 'f32', values: fill(new Float32Array(total))};
    case 'f64':
      return {arrowType: 'f64', values: fill(new Float64Array(total))};
  }
}

/** The name prefix of a points-frame membership column, `membership:<layer>`. @internal */
export const MEMBERSHIP_PREFIX = 'membership:';

/**
 * The per-point highlight column, present only where the request carried a `highlight`. The name is
 * reserved at the build, so no declared render column shares it, and it is skipped by name when
 * decoding declared scalars.
 */
export const HIGHLIGHTED_COLUMN = 'highlighted';

/**
 * Hashes a per-point membership column to a response-local index.
 *
 * The decoder runs in a worker that cannot see the session's artifact ordinals, so it does the
 * per-point work: each distinct `tessera_id` gets a local index from 1, with `0` for null, and the
 * main thread maps the short distinct list to session ordinals (`bands.ts`).
 *
 * Hashed on the two `u32` halves, because a `Map<bigint, …>` allocates a `BigInt` per point. Open
 * addressing over a power-of-two table. Nulls come from each chunk's validity bitmap; a chunk
 * with no bitmap is all valid.
 */
function hashMembership(vectors: Vector<DataType>[], total: number): MembershipColumn {
  const index = total > 0xffff ? new Uint32Array(total) : new Uint16Array(total);
  // At most half full; grown when a response has more distinct ids than expected.
  let capacity = 1 << 12;
  let slotsLo = new Uint32Array(capacity);
  let slotsHi = new Uint32Array(capacity);
  let slotsIdx = new Uint32Array(capacity); // 0 = empty
  let distinctLo: number[] = [];
  let distinctHi: number[] = [];

  const grow = () => {
    capacity *= 2;
    slotsLo = new Uint32Array(capacity);
    slotsHi = new Uint32Array(capacity);
    slotsIdx = new Uint32Array(capacity);
    for (let d = 0; d < distinctLo.length; d++) insert(distinctLo[d]!, distinctHi[d]!, d + 1);
  };
  const insert = (lo: number, hi: number, idx: number) => {
    let at = (Math.imul(lo ^ Math.imul(hi, 0x9e3779b1), 0x85ebca6b) >>> 0) & (capacity - 1);
    while (slotsIdx[at] !== 0) at = (at + 1) & (capacity - 1);
    slotsLo[at] = lo;
    slotsHi[at] = hi;
    slotsIdx[at] = idx;
  };
  const indexOf = (lo: number, hi: number): number => {
    let at = (Math.imul(lo ^ Math.imul(hi, 0x9e3779b1), 0x85ebca6b) >>> 0) & (capacity - 1);
    for (;;) {
      const held = slotsIdx[at]!;
      if (held === 0) {
        distinctLo.push(lo);
        distinctHi.push(hi);
        const idx = distinctLo.length;
        insert(lo, hi, idx);
        if (distinctLo.length * 2 > capacity) grow();
        return idx;
      }
      if (slotsLo[at] === lo && slotsHi[at] === hi) return held;
      at = (at + 1) & (capacity - 1);
    }
  };

  let o = 0;
  for (const vector of vectors) {
    for (const chunk of vector.data) {
      const values = chunk.values as BigUint64Array;
      const halves = new Uint32Array(values.buffer, values.byteOffset, values.length * 2);
      const bitmap = chunk.nullCount > 0 ? chunk.nullBitmap : null;
      const offset = chunk.offset;
      for (let i = 0; i < chunk.length; i++, o++) {
        if (bitmap) {
          const bit = offset + i;
          if (((bitmap[bit >> 3]! >> (bit & 7)) & 1) === 0) continue; // null stays 0
        }
        const at = (offset + i) * 2;
        index[o] = indexOf(halves[at]!, halves[at + 1]!);
      }
    }
  }
  const ids = new BigUint64Array(distinctLo.length);
  for (let d = 0; d < distinctLo.length; d++) {
    ids[d] = BigInt(distinctLo[d]!) | (BigInt(distinctHi[d]!) << 32n);
  }
  return {index, ids};
}

/**
 * Gathers the even bits of a `u32` into the low 16 bits: the inverse of the Morton spread, as
 * `mosaica_build::input::compact`.
 */
function compact(v: number): number {
  let x = v & 0x55555555;
  x = (x | (x >>> 1)) & 0x33333333;
  x = (x | (x >>> 2)) & 0x0f0f0f0f;
  x = (x | (x >>> 4)) & 0x00ff00ff;
  x = (x | (x >>> 8)) & 0x0000ffff;
  return x >>> 0;
}

/**
 * The point half of a response, decoded from one or more kind-3 frames. Each frame is a complete
 * Arrow stream over whole tiles, so the streaming client decodes frames as they arrive; a batch
 * decode is the same function over every frame.
 */
export type PointsPart = {
  ids: BigUint64Array;
  codes: BigUint64Array;
  positions: Float64Array;
  world: Float32Array;
  scalars: Record<string, ScalarColumn>;
  membership: Record<string, MembershipColumn>;
  /** See {@link ViewportResult.highlighted}; null where the frames carried no such column. */
  highlighted: Uint8Array | null;
  /**
   * Which projection the frames were in, read from their schema. `'highlight'` carries
   * `(tessera_id, highlighted)` only: `codes`, `positions`, `world` and `scalars` are empty, and
   * the caller joins the bits to points it holds by `tessera_id`. A caller holding nothing asks
   * again with `'full'`.
   */
  projection: PointsProjection;
};

/** The two shapes a points frame comes in (`point_rows`): a list of columns decodes as `'full'`. */
export type PointsProjection = 'full' | 'highlight';

/**
 * Decodes kind-3 frames into one point block.
 *
 * Arrow JS reads only the first stream of concatenated bytes, so each frame decodes separately and
 * the tables are joined. Zero frames decode to zero points.
 *
 * Each point's `code: uint64` is the Morton interleave of two 32-bit fixed-point axes, and it is
 * de-interleaved here once, into cell space: `[0, 65536)` per axis with a sub-cell fraction, which
 * needs no quantisation extent. The raw `codes` are returned too, since shifting one gives the
 * containing tile at any depth.
 */
export function decodePoints(payloads: readonly Uint8Array[]): PointsPart {
  const pointTables = payloads.map((frame) => tableFromIPC(frame));
  const totalPoints = pointTables.reduce((n, t) => n + t.numRows, 0);
  const ids = new BigUint64Array(totalPoints);
  // The projection is read from the frame's schema, not the request: the highlight projection
  // carries no `code`.
  const projection: PointsProjection = pointTables.length > 0 && pointTables[0]!.getChild('code') == null ? 'highlight' : 'full';
  const codes = new BigUint64Array(projection === 'full' ? totalPoints : 0);
  {
    let offset = 0;
    for (const t of pointTables) {
      ids.set(u64Column(t, 'tessera_id'), offset);
      if (projection === 'full') codes.set(u64Column(t, 'code'), offset);
      offset += t.numRows;
    }
  }
  // `f64`: a cell coordinate is 32 bits per axis, and `f32`'s 24-bit mantissa loses the sub-cell
  // part. Narrowing to `f32` world space happens per band, where that precision is not needed.
  const positions = new Float64Array(projection === 'full' ? ids.length * 2 : 0);
  const world = new Float32Array(projection === 'full' ? ids.length * 2 : 0);
  // Each little-endian `u64` code is read as two `u32` words through a view over the same bytes;
  // reading it as a `BigInt` costs three allocations per point.
  const halves = new Uint32Array(codes.buffer, codes.byteOffset, codes.length * 2);
  for (let i = 0; i < positions.length / 2; i++) {
    // JS bitwise operators are int32, so the arithmetic runs 32 bits at a time. The halves recombine
    // by multiplication, since a shift would overflow int32 at the top of the axis.
    const lo = halves[i * 2]!;
    const hi = halves[i * 2 + 1]!;
    const qx = compact(lo) + compact(hi) * 65536;
    const qy = compact(lo >>> 1) + compact(hi >>> 1) * 65536;
    const x = qx / 65536;
    const y = qy / 65536;
    positions[i * 2] = x;
    positions[i * 2 + 1] = y;
    world[i * 2] = x / CELLS_PER_WORLD_UNIT;
    world[i * 2 + 1] = y / CELLS_PER_WORLD_UNIT;
  }

  // Every frame of a response carries one schema, so the first table's fields are the response's.
  const scalars: Record<string, ScalarColumn> = {};
  const membership: Record<string, MembershipColumn> = {};
  if (pointTables.length > 0) {
    for (const field of pointTables[0]!.schema.fields) {
      if (field.name === 'tessera_id' || field.name === 'code') continue;
      // The highlight bit and the membership columns are not declared scalars; decoded as scalars
      // they would reach the palette and the tooltip.
      if (field.name === HIGHLIGHTED_COLUMN) continue;
      if (field.name.startsWith(MEMBERSHIP_PREFIX)) continue;
      const perFrame = pointTables.map((t) => scalarColumn(field.name, t.getChild(field.name)!));
      scalars[field.name] = concatScalarColumns(perFrame, totalPoints);
    }
    for (const field of pointTables[0]!.schema.fields) {
      if (!field.name.startsWith(MEMBERSHIP_PREFIX)) continue;
      membership[field.name.slice(MEMBERSHIP_PREFIX.length)] = hashMembership(
        pointTables.map((t) => t.getChild(field.name)!),
        totalPoints
      );
    }
  }
  // One byte a point, in the same order as `ids`, so the join to a band is a slice.
  let highlighted: Uint8Array | null = null;
  if (pointTables.length > 0 && pointTables[0]!.schema.fields.some((f) => f.name === HIGHLIGHTED_COLUMN)) {
    highlighted = new Uint8Array(totalPoints);
    let offset = 0;
    for (const t of pointTables) {
      const column = t.getChild(HIGHLIGHTED_COLUMN)!;
      for (let i = 0; i < t.numRows; i++) highlighted[offset + i] = column.get(i) ? 1 : 0;
      offset += t.numRows;
    }
  }
  return {ids, codes, positions, world, scalars, membership, highlighted, projection};
}

/** Decodes the kind-1 frame: every tile's counts, in the response's tile order. */
export function decodeTiles(payload: Uint8Array): TileCounts[] {
  const tileTable = tableFromIPC(payload);
  const tile = u64Column(tileTable, 'tile');
  const visible = u64Column(tileTable, 'visible');
  const matched = u64Column(tileTable, 'matched');
  const served = u64Column(tileTable, 'served');
  // Always present; equal to `matched` where the request carried no highlight.
  const highlighted = u64Column(tileTable, 'highlighted');
  const tiles: TileCounts[] = [];
  for (let i = 0; i < tile.length; i++) {
    tiles.push({
      tile: tile[i]!,
      visible: visible[i]!,
      matched: matched[i]!,
      served: served[i]!,
      highlighted: highlighted[i]!
    });
  }
  return tiles;
}

/** Decodes the kind-2 frame: the underlay's per-cell counts. */
export function decodeSubCells(payload: Uint8Array): SubCell[] {
  const t = tableFromIPC(payload);
  const cell = u64Column(t, 'cell');
  const count = u64Column(t, 'count');
  const subCells: SubCell[] = [];
  for (let i = 0; i < cell.length; i++) {
    subCells.push({cell: cell[i]!, count: count[i]!});
  }
  return subCells;
}

/** The columns of an artifacts frame, at fixed positions. */
const ARTIFACT_COLUMNS = [
  'layer',
  'tessera_id',
  'key',
  'masked_count',
  'centroid_x',
  'centroid_y',
  'box_min_x',
  'box_min_y',
  'box_max_x',
  'box_max_y',
  'content',
  'parent_ids',
  'rung',
  'matched',
  'highlighted',
  'target',
  'tile',
  'slot'
] as const;

/**
 * One kind-5 frame of a `/v1/artifacts/viewport` body: its artifacts, and the tile its rows name,
 * or `null` where it has no row or its rows name none, as the treed frame's do.
 */
export type ArtifactsFramePart = {tile: bigint | null; artifacts: Artifact[]};

/**
 * Decodes one kind-5 frame. Its eighteen columns are checked by name and position first: a
 * column read from the wrong place would draw as data.
 */
export function decodeArtifactsFrame(payload: Uint8Array): ArtifactsFramePart {
  const t = tableFromIPC(payload);
  const names = t.schema.fields.map((f) => f.name);
  if (names.length !== ARTIFACT_COLUMNS.length || names.some((name, i) => name !== ARTIFACT_COLUMNS[i])) {
    throw new Error(`artifacts frame columns are (${names.join(', ')}); this client reads (${ARTIFACT_COLUMNS.join(', ')}), and the server and this client are from different versions`);
  }
  const column = (name: (typeof ARTIFACT_COLUMNS)[number]) => t.getChild(name)!;
  // `layer` is dictionary-encoded (u16 keys over utf8); apache-arrow resolves the dictionary on
  // `.get()`.
  const layer = column('layer');
  const key = column('key');
  const tesseraId = u64Column(t, 'tessera_id');
  const maskedCount = u64Column(t, 'masked_count');
  // Geometry in the same grid units as a point's code. A null means the layer declares none or the
  // request's `computed` left it out; an artifact that could not be served is absent.
  const centroidX = column('centroid_x');
  const centroidY = column('centroid_y');
  const boxMinX = column('box_min_x');
  const boxMinY = column('box_min_y');
  const boxMaxX = column('box_max_x');
  const boxMaxY = column('box_max_y');
  // Positional to the layer's declared content kinds; empty where it declares none.
  const content = column('content');
  // A parent appears only where it is also in this frame, ascending by `tessera_id`. An empty list
  // is a root, a flat artifact or a parent this principal was not served, and these are one value.
  const parentIds = column('parent_ids');
  const rung = column('rung');
  // Null where the request asked no such question, which is not the same as no match.
  const matched = column('matched');
  const highlighted = column('highlighted');
  const target = column('target');
  const tile = column('tile');
  // Null where the request carried no `palette_size`.
  const slot = column('slot');
  const artifacts: Artifact[] = [];
  let frameTile: bigint | null = null;
  for (let i = 0; i < tesseraId.length; i++) {
    const cx = centroidX.get(i);
    const bx = boxMinX.get(i);
    const at = tile.get(i);
    const rowTile = at === null ? null : BigInt(at as number);
    if (i === 0) frameTile = rowTile;
    else if (rowTile !== frameTile) throw new Error(`artifacts frame row ${i} names tile ${rowTile}, and row 0 names ${frameTile}; a frame answers one tile`);
    artifacts.push({
      layer: String(layer.get(i)),
      tesseraId: tesseraId[i]!,
      // A publisher need not supply a key.
      key: key.get(i) === null ? null : String(key.get(i)),
      maskedCount: maskedCount[i]!,
      centroid: cx === null ? null : [Number(cx), Number(centroidY.get(i))],
      box: bx === null ? null : [Number(bx), Number(boxMinY.get(i)), Number(boxMaxX.get(i)), Number(boxMaxY.get(i))],
      content: Array.from(content.get(i) ?? [], (v) => String(v)),
      // A null cell is taken as the empty list, so no parent is invented.
      parentIds: Array.from(parentIds.get(i) ?? [], (v) => BigInt(v as bigint)),
      rung: Number(rung.get(i)),
      matched: matched.get(i) === null ? null : Boolean(matched.get(i)),
      highlighted: highlighted.get(i) === null ? null : Boolean(highlighted.get(i)),
      // Null is attached to nothing. A dependent whose target the response withheld is absent.
      target: target.get(i) === null ? null : BigInt(target.get(i) as bigint),
      slot: slot.get(i) === null ? null : Number(slot.get(i))
    });
  }
  return {tile: frameTile, artifacts};
}

/**
 * The `/v1/artifacts/viewport` trailer, checked against the frames and rows the body carried. Its
 * key set is closed, as the viewport trailer's is.
 */
export function checkArtifactsTrailer(payload: Uint8Array, frames: number, rows: number): Record<string, unknown> {
  const trailer = JSON.parse(new TextDecoder().decode(payload)) as Record<string, unknown>;
  const keys = Object.keys(trailer).sort();
  const expected = ['arrow_serialise_ns', 'frames', 'rows', 'stream_us'];
  if (keys.length !== expected.length || keys.some((k, i) => k !== expected[i])) {
    throw new Error(`artifacts trailer keys outside the closed set: ${keys.join(',')}`);
  }
  if (trailer['frames'] !== frames) throw new Error(`trailer claims ${trailer['frames']} artifacts frames, body carries ${frames}`);
  if (trailer['rows'] !== rows) throw new Error(`trailer claims ${trailer['rows']} artifact rows, body carries ${rows}`);
  return trailer;
}

/**
 * Parses the kind-4 trailer. Its key set is closed, and a key outside it is refused, so the body's
 * one server-written JSON region cannot gain a field no reader checks.
 */
export function parseTrailer(payload: Uint8Array): Record<string, unknown> {
  const trailer = JSON.parse(new TextDecoder().decode(payload)) as Record<string, unknown>;
  const trailerKeys = Object.keys(trailer)
    .filter((k) => k !== 'stage_ns')
    .sort();
  const expected = ['arrow_serialise_ns', 'flushes', 'points', 'stream_us'];
  if (trailerKeys.length !== expected.length || trailerKeys.some((k, i) => k !== expected[i])) {
    throw new Error(`trailer keys outside the closed set: ${trailerKeys.join(',')}`);
  }
  return trailer;
}

/** The trailer's `stage_ns` CSV as numbers in its field order, or `null` where it has none. */
export function stageNsOf(trailer: Record<string, unknown>): number[] | null {
  const csv = trailer['stage_ns'];
  return typeof csv === 'string' ? csv.split(',').map(Number) : null;
}

/**
 * Checks the trailer's point-frame and point counts against the body. A mismatch means the body
 * was mis-framed or edited in transit, so the points in hand are not the answer.
 */
export function checkTrailerCounts(
  trailer: Record<string, unknown>,
  flushes: number,
  points: number
): void {
  if (trailer['flushes'] !== flushes) {
    throw new Error(`trailer claims ${trailer['flushes']} point frames, body carries ${flushes}`);
  }
  if (trailer['points'] !== points) {
    throw new Error(`trailer claims ${trailer['points']} points, body carries ${points}`);
  }
}

/**
 * Decodes a whole `/v1/viewport` body into typed arrays on the calling thread, for a test, a
 * script, a counts-only request or any caller holding a whole body. `ids` stays a
 * `BigUint64Array`, since a `u64` does not survive conversion to a double.
 *
 * @throws `Error` for a body that is truncated, lacks its tiles frame or trailer, has a frame of
 *   unknown kind or out of order, carries a column this client cannot read, or whose trailer counts
 *   disagree with its frames.
 *
 * @category HTTP client
 */
export function decodeViewport(body: Uint8Array): ViewportResult {
  const parts = splitFramedStreams(body);
  const trailer = parseTrailer(parts.trailer);
  const tiles = decodeTiles(parts.tiles);
  const {ids, codes, positions, world, scalars, membership, highlighted, projection} = decodePoints(parts.points);
  checkTrailerCounts(trailer, parts.points.length, ids.length);
  const subCells = parts.subCells ? decodeSubCells(parts.subCells) : null;
  return {tiles, ids, codes, positions, world, scalars, membership, highlighted, pointsProjection: projection, subCells};
}
