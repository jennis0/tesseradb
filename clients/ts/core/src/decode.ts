import {tableFromIPC, Type, type DataType, type Table, type Vector} from 'apache-arrow';
import {CELLS_PER_WORLD_UNIT} from './coords.js';
import {splitFramedStreams} from './frame.js';
import type {Artifact, ArtifactIdentity, MembershipColumn, ScalarColumn, SubCell, TileCounts, ViewportResult} from './types.js';

function u64Column(table: Table, name: string): BigUint64Array {
  const col = table.getChild(name);
  if (!col) throw new Error(`viewport payload has no column "${name}"`);
  return col.toArray() as BigUint64Array;
}

/** One ring of a hull on one axis: `uint32` vertex coordinates in grid units. */
type Ring = {length: number; get(v: number): number | null};
/** One artifact's rings on one axis — the outer list of `list<list<uint32>>`. */
type Rings = {length: number; get(r: number): Ring | null};

/**
 * A hull axis column, checked to be `list<list<uint32>>` before a row is read — or `null` where
 * the schema carries no such column at all.
 *
 * **Absence is a schema fact, not a version skew** (contracts §3.2 r44): the two hull columns
 * trail the fixed prefix and are omitted entirely when no served layer declares a hull, an absent
 * column being distinguishable from a null one so decision 0076's rule — a null means *this
 * layer declares no such property*, never *withheld* — gains no third reading.
 *
 * **Where the column is present, the nesting is the contract, and it is verified at the schema
 * rather than discovered at the first row** (contracts §3.2 item 4). A pre-r40 server sends one
 * flat `list<uint32>` per artifact; read two levels deep that column yields a number where a ring
 * is expected, and the ring would come out as a single vertex on a shape with none of the
 * artifact's ground. That mismatch is a version skew between this client and the service it is
 * talking to, so it is a refusal with the two types named and not a shape to accommodate.
 */
function ringColumn(table: Table, name: string): {get(i: number): Rings | null} | null {
  const col = table.getChild(name);
  if (!col) return null;
  const inner = (col.type as {children?: {type: DataType}[]}).children?.[0]?.type;
  if (col.type.typeId !== Type.List || inner?.typeId !== Type.List) {
    throw new Error(
      `viewport payload column "${name}" is ${col.type} — a served hull is a list of rings, ` +
        'so the column is list<list<uint32>> (contracts §3.2 item 4). A flat list is a server ' +
        'older than the rings change.'
    );
  }
  return col as unknown as {get(i: number): Rings | null};
}

/**
 * One declared-scalar column, as the type the manifest declared plus the buffer Arrow already
 * holds.
 *
 * **`toArray()`, not a spread.** Arrow's numeric children are typed arrays already; `[...child]`
 * boxes every element, which at 5 × 10⁴ marks across the wide fixture's eighteen columns is close
 * to a million throwaway heap objects per response. `bool` and `utf8` have no typed form — a
 * bitmap and an offset table respectively — so those two, and only those two, materialise.
 *
 * **A type the manifest cannot declare is a decoder bug, not a column to skip.** Silently dropping
 * it would shift nothing (the map is keyed by name) but would make the column vanish from the
 * legend with no error, so it throws instead. The thirteen arms mirror
 * `tessera_wire::payload::ScalarColumn`; the two must be changed together.
 */
function scalarColumn(name: string, vector: Vector<DataType>): ScalarColumn {
  const type = vector.type;
  switch (type.typeId) {
    case Type.Bool:
      return {arrowType: 'bool', values: [...vector] as boolean[]};
    case Type.Utf8:
      return {arrowType: 'utf8', values: [...vector] as string[]};
    case Type.Timestamp:
      return {arrowType: 'timestamp_us', values: vector.toArray() as BigInt64Array};
    case Type.Int: {
      // `Int` covers all eight widths; the bit width and signedness are on the type, not the id.
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
 * Concatenate one declared column's per-frame pieces into a single [`ScalarColumn`].
 *
 * All pieces carry the same `arrowType` by construction — every frame serialises the same
 * manifest schema — asserted rather than assumed, because a mismatch would mean the fill below
 * silently mixed two columns' values. Typed-array families allocate once and `set`; `bool` and
 * `utf8` are plain JS arrays and concat.
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
  if (first.arrowType === 'bool' || first.arrowType === 'utf8') {
    return {
      arrowType: first.arrowType,
      values: pieces.flatMap((p) => p.values as (boolean | string)[])
    } as ScalarColumn;
  }
  // The typed families share the `set`-into-a-preallocated-buffer shape; the switch is what
  // names each concrete constructor for the type checker. Mirrors `scalarColumn`'s arms — the
  // two must be changed together.
  const fill = <A extends {set(a: A, o: number): void; length: number}>(out: A): A => {
    let offset = 0;
    for (const piece of pieces) {
      // Same `arrowType` (asserted above) means same concrete typed-array class; the checker
      // cannot see through the union, hence the `unknown` step.
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

/** The points-frame column name prefix a membership column carries (contracts §3.2, r39). */
export const MEMBERSHIP_PREFIX = 'membership:';

/**
 * The per-point membership column, hashed to a **response-local index** (design §5.10).
 *
 * The decoder runs in a worker lane that shares nothing with the other lanes, so it cannot name
 * an artifact with a session ordinal; what it can do is the per-point work. Each distinct
 * `tessera_id` the column carries — at most the response's served artifacts, ≤ 10⁴ — gets a
 * local index from 1, `0` standing for null, and the main thread maps the short distinct list to
 * session ordinals and remaps the index array with a tight loop (`bands.ts`).
 *
 * **Hashed on the two `u32` halves, never on a `BigInt`.** Arrow's `u64` column is little-endian
 * words already; a `Map<bigint, …>` would allocate a `BigInt` per point, which at 10⁶ points is
 * the same per-point allocation `decode.ts` refuses for the position code. Open addressing over a
 * power-of-two table sized to the point count, so the probe sequence is bounded by load.
 *
 * Nulls are read from each chunk's validity bitmap; a chunk with no bitmap is all valid.
 */
function hashMembership(vectors: Vector<DataType>[], total: number): MembershipColumn {
  const index = total > 0xffff ? new Uint32Array(total) : new Uint16Array(total);
  // Distinct ids as their halves; `capacity` is a power of two at most half full for ≤ 10⁴
  // distinct, and grown when a response defies that.
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
          if (((bitmap[bit >> 3]! >> (bit & 7)) & 1) === 0) continue; // null → 0, already
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
 * Gather the even bits of a `u32` into the low 16 bits — the inverse of the Morton spread, and the
 * mirror of `tessera_build::input::compact`.
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
 * The point half of a response, decoded from one or more kind-3 frames.
 *
 * Separated from {@link ViewportResult} because the points are the part that arrives in pieces:
 * a wide response is dozens of frames, each a complete Arrow stream over whole tiles, and the
 * streaming client decodes them one at a time as the wire delivers them. A batch decode is the
 * same function over every frame at once.
 */
export type PointsPart = {
  ids: BigUint64Array;
  codes: BigUint64Array;
  positions: Float64Array;
  world: Float32Array;
  scalars: Record<string, ScalarColumn>;
  membership: Record<string, MembershipColumn>;
};

/**
 * Decode kind-3 frames into one point block.
 *
 * Each frame is a complete Arrow stream; Arrow JS reads the batches of one stream into one Table,
 * and concatenating the frames' streams byte-wise would decode only the first — so frames decode
 * separately and their tables concatenate as row groups. Zero frames (an empty response carries no
 * points schema at all — contracts §3.2) decodes to zero points.
 *
 * **Positions are deinterleaved here, once**, into the layout deck.gl's `getPosition` wants. The
 * server ships one `code: uint64` per point — the Morton interleave of two 32-bit fixed-point
 * axes — so this is where the axes come apart. The output is **cell space**, `[0, 65536)` per axis
 * with a fraction below the cell, which is the grid's own units and needs no quantisation extent to
 * interpret; the raw `codes` are returned alongside, because a client that wants the containing
 * tile at any depth gets it by shifting rather than by re-quantising.
 */
export function decodePoints(payloads: readonly Uint8Array[]): PointsPart {
  const pointTables = payloads.map((frame) => tableFromIPC(frame));
  const totalPoints = pointTables.reduce((n, t) => n + t.numRows, 0);
  const ids = new BigUint64Array(totalPoints);
  const codes = new BigUint64Array(totalPoints);
  {
    let offset = 0;
    for (const t of pointTables) {
      ids.set(u64Column(t, 'tessera_id'), offset);
      codes.set(u64Column(t, 'code'), offset);
      offset += t.numRows;
    }
  }
  // **`f64`, and not because it is convenient.** A cell coordinate is 32 bits per axis, so the
  // de-interleaved value needs a 32-bit mantissa to round-trip; `f32` has 24 and loses the
  // sub-cell part. `decode.test.ts` re-interleaves these back into the server's `code` and would
  // catch it. The narrowing to the renderer's `f32` world space happens later, per band, where the
  // precision is no longer needed.
  const positions = new Float64Array(ids.length * 2);
  const world = new Float32Array(ids.length * 2);
  // **The halves are read as `u32`s over the same bytes, never as `BigInt`s.** Arrow's `u64` column
  // is little-endian, so each code is already two 32-bit words in the order this loop wants them,
  // and a `Uint32Array` view costs nothing. Taking them off the `BigUint64Array` instead — one read
  // plus a shift plus a mask — is three `BigInt` allocations per point, which at 2.5 × 10^6 points
  // measured 2.5 s of decode on the main thread and was the largest single cost in the client.
  const halves = new Uint32Array(codes.buffer, codes.byteOffset, codes.length * 2);
  for (let i = 0; i < ids.length; i++) {
    // JS bitwise operators are int32, so the spread/compact arithmetic happens 32 bits at a time.
    // The halves recombine by multiplication rather than by shifting, which would overflow int32
    // at the top of the axis.
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

  // Scalars concatenate per column across the frames' tables. Every frame carries the full
  // declared schema (each is a complete stream over the same manifest), so the first table's
  // field list is the response's.
  const scalars: Record<string, ScalarColumn> = {};
  const membership: Record<string, MembershipColumn> = {};
  if (pointTables.length > 0) {
    for (const field of pointTables[0]!.schema.fields) {
      if (field.name === 'tessera_id' || field.name === 'code') continue;
      // **The per-point membership column is not a declared scalar** — it is the deepest served
      // artifact per named layer (D12, §5.10), a nullable `u64` named `membership:<layer>` after
      // the render scalars. It is hashed below into a response-local index; here it is skipped by
      // name so it is never coloured by, ranked, or shown as a column. Decoding it as a scalar
      // would put a `tessera_id` on the palette.
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
  return {ids, codes, positions, world, scalars, membership};
}

/** Decode the kind-1 frame: every tile's counts, in the response's own tile order. */
export function decodeTiles(payload: Uint8Array): TileCounts[] {
  const tileTable = tableFromIPC(payload);
  const tile = u64Column(tileTable, 'tile');
  const visible = u64Column(tileTable, 'visible');
  const matched = u64Column(tileTable, 'matched');
  const served = u64Column(tileTable, 'served');
  const tiles: TileCounts[] = [];
  for (let i = 0; i < tile.length; i++) {
    tiles.push({
      tile: tile[i]!,
      visible: visible[i]!,
      matched: matched[i]!,
      served: served[i]!
    });
  }
  return tiles;
}

/** Decode the kind-2 frame: the underlay's per-cell counts. */
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
 * Decode the kind-5 frame, in whichever projection the server sent.
 *
 * The projection is read off the frame's own schema, never off the request: the identity frame is
 * exactly the four columns `(layer, tessera_id, rung, matched)` (contracts §3.2 r44), the full
 * frame's fixed prefix is fourteen with the two hull columns trailing.
 */
export function decodeArtifactsFrame(payload: Uint8Array): {
  artifacts: Artifact[];
  artifactsIdentity: ArtifactIdentity[] | null;
} {
  const artifacts: Artifact[] = [];
  const t = tableFromIPC(payload);
  // `layer` is dictionary-encoded (u16 keys over utf8, contracts §3.2 r44) in both projections.
  // apache-arrow resolves the dictionary on `.get()` — the vector hands back the utf8 value,
  // never the key — so the column reads exactly as the plain-utf8 encoding did; verified by
  // test rather than assumed (`artifacts-frame.test.ts`).
  const layer = t.getChild('layer')!;
  if (t.schema.fields.length === 4) {
    const tesseraId = u64Column(t, 'tessera_id');
    const rung = t.getChild('rung');
    const matched = t.getChild('matched');
    if (rung == null || matched == null) {
      throw new Error(
        'viewport artifacts frame has four columns but is not the identity projection: expected (layer, tessera_id, rung, matched)'
      );
    }
    const artifactsIdentity: ArtifactIdentity[] = [];
    for (let i = 0; i < tesseraId.length; i++) {
      artifactsIdentity.push({
        layer: String(layer.get(i)),
        tesseraId: tesseraId[i]!,
        rung: Number(rung.get(i)),
        matched: matched.get(i) === null ? null : Boolean(matched.get(i))
      });
    }
    return {artifacts, artifactsIdentity};
  }
  const key = t.getChild('key')!;
  const tesseraId = u64Column(t, 'tessera_id');
  const maskedCount = u64Column(t, 'masked_count');
  // Derived geometry, in the same grid units as `codes` — no extent needed to draw it. A null is
  // *this layer declares none*, never *withheld*: an artifact whose content could not be served
  // does not appear at all.
  const centroidX = t.getChild('centroid_x')!;
  const centroidY = t.getChild('centroid_y')!;
  const boxMinX = t.getChild('box_min_x')!;
  const boxMinY = t.getChild('box_min_y')!;
  const boxMaxX = t.getChild('box_max_x')!;
  const boxMaxY = t.getChild('box_max_y')!;
  // `hull_x` and `hull_y` are `list<list<uint32>>` — **one entry per ring** (contracts §3.2
  // item 4, `artifact-shapes.md` §9) — and **trail the fixed prefix, absent from the schema
  // entirely when no served layer declares a hull** (r44). Read by name, tolerating absence:
  // an absent pair reads as no artifact carrying a hull, and a per-row null in a present pair
  // keeps its one meaning (the layer declares none). Where a column is present, the nesting is
  // checked at the schema, so a body from a server that still sends one flat ring per artifact
  // is refused rather than misread: the downcast is what a single-ring reader fails on, and the
  // same downcast in reverse is what this decoder must not paper over.
  const hullX = ringColumn(t, 'hull_x');
  const hullY = ringColumn(t, 'hull_y');
  // The two travel together by contract; one without the other has no reading.
  if ((hullX === null) !== (hullY === null)) {
    throw new Error('viewport artifacts frame carries one hull column and not the other');
  }
  // One content, entire, positional to the layer's declared kinds. Empty means the layer
  // declares no supplied content — never that content was withheld, because an artifact whose
  // content this principal may not read does not appear at all.
  const content = t.getChild('content')!;
  // **Present only where the parent is also in this response.** A null is a root *or* a parent
  // this principal was not served, and the two are deliberately one value: naming the second
  // would disclose that a coarser artifact exists which they may not see. Read it as "no parent
  // here", never as "no parent".
  const parentId = t.getChild('parent_id');
  // **The rung a client draws this artifact at**, non-nullable, computed the right way for the
  // layer's kind (contracts §3.2 r44): the declared level on a levelled layer, the
  // response-local parent-chain depth on a treed one, 0 on a flat one. Read by name like every
  // other column here; the schema's *position* is contract for a decoder that indexes
  // positionally, which this one deliberately is not.
  const rung = t.getChild('rung');
  // **The filter bit, and null is a value**: the column is all-null where the request carried no
  // filter, which is *there was no question* rather than *no matches* (decision 0104). A missing
  // column reads the same way, and unlike `rung` there is nothing to refuse over — a client that
  // asked for no filter has no use for it, and one that did draws every artifact undimmed, which
  // is what it drew before the column existed.
  const matched = t.getChild('matched');
  // **A loud refusal rather than a guessed zero.** There is no compatibility to keep here
  // (decision 0048) and the rung is what a client draws every layer's resolution from, so a
  // body without the column — an r41-or-earlier server's `level` included — is a server this
  // build does not match: silently reading every artifact as rung 0 would draw the whole
  // hierarchy at its coarsest and look like data.
  if (rung == null) {
    throw new Error(
      'viewport artifacts frame carries no `rung` column: this client requires a server that serves it (contracts §3.2 r44 renamed and re-meant `level`)'
    );
  }
  for (let i = 0; i < tesseraId.length; i++) {
    const cx = centroidX.get(i);
    const bx = boxMinX.get(i);
    const hx = hullX === null ? null : hullX.get(i);
    const hy = hullY === null ? null : hullY.get(i);
    // The two axes carry the same ring structure by construction. **Checked, not assumed** — a
    // decoder that assumes it misdraws silently on the day something else does not, and a ring
    // whose axes disagree has no reading at all: a shorter x than y would draw a ring that
    // closes early, in the shape of a real boundary.
    if ((hx === null) !== (hy === null)) {
      throw new Error(`viewport artifact row ${i}: one hull axis is null and the other is not`);
    }
    let hull: [number, number][][] | null = null;
    if (hx !== null && hy !== null) {
      if (hx.length !== hy.length) {
        throw new Error(`viewport artifact row ${i}: hull axes disagree on ring count (${hx.length} and ${hy.length})`);
      }
      hull = [];
      for (let r = 0; r < hx.length; r++) {
        const rx = hx.get(r);
        const ry = hy.get(r);
        if (rx === null || ry === null) {
          throw new Error(`viewport artifact row ${i}: hull ring ${r} is null on one axis`);
        }
        if (rx.length !== ry.length) {
          throw new Error(`viewport artifact row ${i}: hull axes disagree on the length of ring ${r} (${rx.length} and ${ry.length})`);
        }
        const ring: [number, number][] = [];
        for (let v = 0; v < rx.length; v++) ring.push([Number(rx.get(v)), Number(ry.get(v))]);
        hull.push(ring);
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
      hull,
      content: Array.from(content.get(i) ?? [], (v) => String(v)),
      // Absent on a server older than the field, which reads the same as a root — the
      // fail-closed direction, and the only one available without inventing a parent.
      parentId: parentId == null || parentId.get(i) === null ? null : BigInt(parentId.get(i)),
      rung: Number(rung.get(i)),
      matched: matched == null || matched.get(i) === null ? null : Boolean(matched.get(i))
    });
  }
  return {artifacts, artifactsIdentity: null};
}

/**
 * A response's head: the frames the server sends before any points frame.
 *
 * The counts channel, the underlay and the artifacts all land in the first flush
 * (`streamed-serving.md` §3), so this is everything a client can draw before a single point has
 * arrived — and, for a client naming layers, everything a point's membership column is named
 * through.
 */
export type ViewportHead = {
  tiles: TileCounts[];
  subCells: SubCell[] | null;
  artifacts: Artifact[];
  artifactsIdentity: ArtifactIdentity[] | null;
};

/** Decode the head frames together — the one unit the streaming client asks its decoder for. */
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
 * Parse the kind-4 trailer, refusing any key outside the closed set.
 *
 * The trailer's key set is closed (contracts §3.2 r26) and validated at every decode: the one
 * server-authored JSON region of the body must not quietly acquire a field no reader checks.
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
 * The trailer's two counts against what the body actually carried.
 *
 * The server states how many point frames it flushed and how many points it served; a body that
 * disagrees is a body a reader has mis-framed or a transport has edited, and either way the points
 * in hand are not the answer to the question asked.
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
 * Decode a framed `/v1/viewport` body, whole.
 *
 * Two rules this path exists to hold:
 *
 * 1. `ids` stays a `BigUint64Array`. A `tessera_id` is a u64 and does not survive a double.
 * 2. Positions are deinterleaved once, in {@link decodePoints}, into the renderer's layout.
 *
 * **The streaming client does not come through here** — it takes the same frames one at a time as
 * the wire delivers them (`client.ts`) so that a tile can be drawn before the last byte lands.
 * This is the batch surface: a test, a script, a counts-only ask, and anything holding a whole
 * body already.
 */
export function decodeViewport(body: Uint8Array): ViewportResult {
  const parts = splitFramedStreams(body);
  const trailer = parseTrailer(parts.trailer);
  const tiles = decodeTiles(parts.tiles);
  const {ids, codes, positions, world, scalars, membership} = decodePoints(parts.points);
  checkTrailerCounts(trailer, parts.points.length, ids.length);
  const subCells = parts.subCells ? decodeSubCells(parts.subCells) : null;
  // Empty when the response carried no artifacts frame, which is the ordinary state of a
  // deployment with no layers — and of a principal who reaches none, and of a view holding none.
  // Those are one answer on purpose; see `Artifact`.
  const {artifacts, artifactsIdentity} = parts.artifacts
    ? decodeArtifactsFrame(parts.artifacts)
    : {artifacts: [] as Artifact[], artifactsIdentity: null};

  return {tiles, ids, codes, positions, world, scalars, membership, subCells, artifacts, artifactsIdentity};
}
