import {tableFromIPC, Type, type DataType, type Table, type Vector} from 'apache-arrow';
import {CELLS_PER_WORLD_UNIT} from './coords.js';
import {splitFramedStreams} from './frame.js';
import type {Artifact, ArtifactIdentity, MembershipColumn, ScalarColumn, ScalarValues, Shape, SubCell, TileCounts, ViewportResult} from './types.js';

function u64Column(table: Table, name: string): BigUint64Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as BigUint64Array;
}

/** One ring of a shape on one axis: `uint32` vertex coordinates in grid units. */
type Ring = {length: number; get(v: number): number | null};
/** One part's rings on one axis. */
type Rings = {length: number; get(r: number): Ring | null};
/** One artifact's parts on one axis: the outer list of `list<list<list<uint32>>>`. */
type Parts = {length: number; get(p: number): Rings | null};

/**
 * A shape axis column, checked to be `list<list<list<uint32>>>` (parts, rings, vertices) before a
 * row is read, or `null` where the schema has no such column. The shape columns are omitted when no
 * served layer draws a shape. A column two levels deep comes from an older server; read three
 * levels deep it would draw a second part as a hole of the first, so it is refused.
 */
function partsColumn(table: Table, name: string): {get(i: number): Parts | null} | null {
  const col = table.getChild(name);
  if (!col) return null;
  const inner = (col.type as {children?: {type: DataType}[]}).children?.[0]?.type;
  const innermost = (inner as {children?: {type: DataType}[]} | undefined)?.children?.[0]?.type;
  if (col.type.typeId !== Type.List || inner?.typeId !== Type.List || innermost?.typeId !== Type.List) {
    throw new Error(
      `viewport artifacts column "${name}" is ${col.type}; a shape column is ` +
        'list<list<list<uint32>>> (parts, rings, vertices), which a server older than this client does not send.'
    );
  }
  return col as unknown as {get(i: number): Parts | null};
}

/**
 * One declared scalar column: the declared type and the buffer Arrow already holds.
 *
 * Numeric columns use `toArray()`, which returns Arrow's typed array; a spread would box every
 * element. `bool` and `utf8` have no typed form and are materialised. A type the manifest cannot
 * declare throws, since skipping it would drop the column with no error. The arms mirror
 * `tessera_wire::payload::ScalarColumn`, and the two change together. Nulls are read into
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
 * `tessera_build::input::compact`.
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

/** The two shapes a points frame comes in (`point_rows`). */
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

  // Every frame carries the full declared schema, so the first table's fields are the response's.
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

/**
 * Decodes the kind-5 frame in whichever projection the server sent, read from the frame's schema.
 * The identity frame is exactly `(layer, tessera_id, rung, matched, highlighted)`; the full frame
 * has sixteen fixed columns with the two shape columns after them.
 */
export function decodeArtifactsFrame(payload: Uint8Array): {
  artifacts: Artifact[];
  artifactsIdentity: ArtifactIdentity[] | null;
} {
  const artifacts: Artifact[] = [];
  const t = tableFromIPC(payload);
  // `layer` is dictionary-encoded (u16 keys over utf8); apache-arrow resolves the dictionary on
  // `.get()`.
  const layer = t.getChild('layer')!;
  if (t.schema.fields.length === 5) {
    const tesseraId = u64Column(t, 'tessera_id');
    const rung = t.getChild('rung');
    const matched = t.getChild('matched');
    const highlighted = t.getChild('highlighted');
    if (rung == null || matched == null || highlighted == null) {
      throw new Error(
        'viewport artifacts frame has five columns but is not the identity projection: expected (layer, tessera_id, rung, matched, highlighted)'
      );
    }
    const artifactsIdentity: ArtifactIdentity[] = [];
    for (let i = 0; i < tesseraId.length; i++) {
      artifactsIdentity.push({
        layer: String(layer.get(i)),
        tesseraId: tesseraId[i]!,
        rung: Number(rung.get(i)),
        matched: matched.get(i) === null ? null : Boolean(matched.get(i)),
        highlighted: highlighted.get(i) === null ? null : Boolean(highlighted.get(i))
      });
    }
    return {artifacts, artifactsIdentity};
  }
  const key = t.getChild('key')!;
  const tesseraId = u64Column(t, 'tessera_id');
  const maskedCount = u64Column(t, 'masked_count');
  // Derived geometry, in the same grid units as `codes`. A null means the layer declares none; an
  // artifact whose content could not be served is absent.
  const centroidX = t.getChild('centroid_x')!;
  const centroidY = t.getChild('centroid_y')!;
  const boxMinX = t.getChild('box_min_x')!;
  const boxMinY = t.getChild('box_min_y')!;
  const boxMaxX = t.getChild('box_max_x')!;
  const boxMaxY = t.getChild('box_max_y')!;
  // `shape_x` and `shape_y` are read by name and may be absent, which reads as no artifact having a
  // shape; a null row in a present pair means the layer draws none. The old `hull_x`/`hull_y`
  // names are refused: read as absent, every cluster would draw as its box.
  if (t.getChild('hull_x') || t.getChild('hull_y')) {
    throw new Error(
      'viewport artifacts frame carries `hull_x`/`hull_y` from a server older than this client; it expects `shape_x`/`shape_y` as parts of rings.'
    );
  }
  const shapeX = partsColumn(t, 'shape_x');
  const shapeY = partsColumn(t, 'shape_y');
  // The two travel together; one alone has no reading.
  if ((shapeX === null) !== (shapeY === null)) {
    throw new Error('viewport artifacts frame carries one shape column and not the other');
  }
  // The content, positional to the layer's declared kinds. Empty means the layer declares no
  // supplied content: an artifact whose content this principal may not read is absent.
  const content = t.getChild('content')!;
  // A parent appears only where it is also in this response, ascending by `tessera_id`. An empty
  // list is a root, a flat artifact or a parent this principal was not served, and these are one
  // value: telling the last apart would disclose an artifact they may not see. A tree serves at
  // most one entry, a `dag` layer several.
  const parentIds = t.getChild('parent_ids');
  // The rung the client draws this artifact at: the declared level on a levelled layer, the
  // parent-chain depth in this response on a treed one, 0 on a flat one.
  const rung = t.getChild('rung');
  // The filter bit. All null, or absent, where the request carried no filter, meaning no question
  // was asked rather than no matches.
  const matched = t.getChild('matched');
  // The highlight bit, read the same way for the highlight.
  const highlighted = t.getChild('highlighted');
  // The `tessera_id` of the artifact in this frame this row is attached to, or null.
  const target = t.getChild('target');
  // A missing `rung`, `parent_ids` or `target` is a server this client does not match, and is
  // refused: read as zero, empty or null it would draw the hierarchy flat or drop every attached
  // label, and look like data.
  if (rung == null) {
    throw new Error(
      'viewport artifacts frame has no `rung` column; this client expects one on every artifact row, which a server older than this client does not send.'
    );
  }
  if (parentIds == null) {
    throw new Error(
      'viewport artifacts frame has no `parent_ids` column; this client expects a list of parent ids on every artifact row, which a server older than this client does not send.'
    );
  }
  if (target == null) {
    throw new Error(
      'viewport artifacts frame has no `target` column; this client expects the attached artifact\'s `tessera_id`, or null, on every artifact row, which a server older than this client does not send.'
    );
  }
  for (let i = 0; i < tesseraId.length; i++) {
    const cx = centroidX.get(i);
    const bx = boxMinX.get(i);
    const sx = shapeX === null ? null : shapeX.get(i);
    const sy = shapeY === null ? null : shapeY.get(i);
    // The two axes are checked for the same structure at every level: a shorter x than y would draw
    // a ring that closes early.
    if ((sx === null) !== (sy === null)) {
      throw new Error(`viewport artifact row ${i}: one shape axis is null and the other is not`);
    }
    let shape: Shape | null = null;
    if (sx !== null && sy !== null) {
      if (sx.length !== sy.length) {
        throw new Error(`viewport artifact row ${i}: shape axes disagree on part count (${sx.length} and ${sy.length})`);
      }
      shape = [];
      for (let p = 0; p < sx.length; p++) {
        const px = sx.get(p);
        const py = sy.get(p);
        if (px === null || py === null) {
          throw new Error(`viewport artifact row ${i}: shape part ${p} is null on one axis`);
        }
        if (px.length !== py.length) {
          throw new Error(`viewport artifact row ${i}: shape axes disagree on the ring count of part ${p} (${px.length} and ${py.length})`);
        }
        const rings: [number, number][][] = [];
        for (let r = 0; r < px.length; r++) {
          const rx = px.get(r);
          const ry = py.get(r);
          if (rx === null || ry === null) {
            throw new Error(`viewport artifact row ${i}: shape ring ${r} of part ${p} is null on one axis`);
          }
          if (rx.length !== ry.length) {
            throw new Error(`viewport artifact row ${i}: shape axes disagree on the length of ring ${r} of part ${p} (${rx.length} and ${ry.length})`);
          }
          const ring: [number, number][] = [];
          for (let v = 0; v < rx.length; v++) ring.push([Number(rx.get(v)), Number(ry.get(v))]);
          rings.push(ring);
        }
        shape.push(rings);
      }
    }
    artifacts.push({
      layer: String(layer.get(i)),
      tesseraId: tesseraId[i]!,
      // A publisher need not supply a key.
      key: key.get(i) === null ? null : String(key.get(i)),
      maskedCount: maskedCount[i]!,
      centroid: cx === null ? null : [Number(cx), Number(centroidY.get(i))],
      box:
        bx === null
          ? null
          : [Number(bx), Number(boxMinY.get(i)), Number(boxMaxX.get(i)), Number(boxMaxY.get(i))],
      shape,
      content: Array.from(content.get(i) ?? [], (v) => String(v)),
      // A null cell is taken as the empty list, so no parent is invented.
      parentIds: Array.from(parentIds.get(i) ?? [], (v) => BigInt(v as bigint)),
      rung: Number(rung.get(i)),
      matched: matched == null || matched.get(i) === null ? null : Boolean(matched.get(i)),
      highlighted: highlighted == null || highlighted.get(i) === null ? null : Boolean(highlighted.get(i)),
      // Null is attached to nothing. A dependent whose target this response withheld is absent.
      target: target.get(i) === null ? null : BigInt(target.get(i) as bigint)
    });
  }
  return {artifacts, artifactsIdentity: null};
}

/**
 * A response's head: the frames the server sends before any points frame, all in the first flush.
 * It is everything a client can draw before a point arrives, and what membership columns are
 * named through.
 */
export type ViewportHead = {
  tiles: TileCounts[];
  subCells: SubCell[] | null;
  artifacts: Artifact[];
  artifactsIdentity: ArtifactIdentity[] | null;
};

/** Decodes the head frames together, as the streaming client asks its decoder to. */
export function decodeHead(frames: {
  tiles: Uint8Array;
  subCells: Uint8Array | null;
  artifacts: Uint8Array | null;
}): ViewportHead {
  const {artifacts, artifactsIdentity} = frames.artifacts
    ? decodeArtifactsFrame(frames.artifacts)
    : {artifacts: [] as Artifact[], artifactsIdentity: null};
  return {
    tiles: decodeTiles(frames.tiles),
    subCells: frames.subCells ? decodeSubCells(frames.subCells) : null,
    artifacts,
    artifactsIdentity
  };
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
  // No artifacts frame means no layers, no layer this principal reaches, or none in view; these
  // are one answer.
  const {artifacts, artifactsIdentity} = parts.artifacts
    ? decodeArtifactsFrame(parts.artifacts)
    : {artifacts: [] as Artifact[], artifactsIdentity: null};

  return {tiles, ids, codes, positions, world, scalars, membership, highlighted, pointsProjection: projection, subCells, artifacts, artifactsIdentity};
}
