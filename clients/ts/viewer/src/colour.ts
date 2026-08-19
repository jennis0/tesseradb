/**
 * Turning a declared column into mark colour.
 *
 * **The boundary this file sits on** (client-interaction §9): *if it renders as a number, it is
 * served exact; if it renders as a mapping — colour, size, transfer function — the client may
 * compute it.* Nothing here is a masked quantity and nothing here decides what is drawn. Every
 * served mark gets a colour, always; an unresolvable value gets grey rather than being dropped,
 * because dropping it would make this file a second selection rule and break I7.
 *
 * **The domain a numeric ramp spans is derived from the marks actually served**, never published
 * by the server. A corpus-wide min/max would be an aggregate over items the viewer cannot see —
 * the one trap the design names by name — so the ramp rescales as the viewport moves, and the
 * legend says so rather than hiding it. It is **sticky**: widened as new marks arrive, never
 * narrowed, so a pan does not recolour the whole map under the user. It resets on principal
 * change, a different mask being a different domain.
 */
import type {CategoryValue, ScalarColumn} from '@tessera/client';

/** RGBA, 0–255, the form deck.gl's binary `getFillColor` attribute wants. */
type Rgba = readonly [number, number, number, number];

const ALPHA = 200;

/** The uniform colour, and what the map looked like before any of this existed. */
export const UNIFORM: Rgba = [120, 190, 255, ALPHA];

/**
 * Everything unmappable: the *absent* sentinel (code 0), a code no key explains, and any value
 * past the palette's end.
 *
 * **Deliberately legible rather than invisible.** These marks are served and must be drawn (I7),
 * so the colour has to read as "no value" without reading as "no mark" — a low-saturation grey
 * that recedes behind the palette without disappearing against the background.
 */
export const UNMAPPED: Rgba = [110, 118, 132, ALPHA];

/**
 * A qualitative palette, indexed by a value's **rank among the resolved values ordered by key** —
 * never by its code.
 *
 * Codes are drawn at random from the declared width (per-point-attributes §3.4), so indexing by
 * code would need a 65,536-entry palette for a `u16` and would colour adjacent legend entries
 * arbitrarily. Rank is dense, stable for a session, and puts the palette's most distinguishable
 * colours on the values a reader sees first.
 *
 * Twelve hues, ordered so that neighbours differ in both hue and lightness — adjacent legend
 * entries are the pairs a reader most needs to tell apart, and hue alone fails for the ~8% of men
 * with a red/green deficiency.
 */
const PALETTE: readonly Rgba[] = [
  [102, 194, 255, ALPHA],
  [255, 158, 74, ALPHA],
  [122, 214, 148, ALPHA],
  [237, 118, 137, ALPHA],
  [190, 160, 255, ALPHA],
  [214, 178, 118, ALPHA],
  [246, 138, 214, ALPHA],
  [150, 210, 214, ALPHA],
  [222, 214, 110, ALPHA],
  [138, 166, 246, ALPHA],
  [246, 178, 158, ALPHA],
  [166, 206, 110, ALPHA]
];

export const PALETTE_SIZE = PALETTE.length;

/** A numeric column's colour ramp: min → max, low to high. */
const RAMP_LOW: Rgba = [40, 60, 140, ALPHA];
const RAMP_HIGH: Rgba = [255, 214, 120, ALPHA];

/** The sticky domain for one numeric column. */
export type Domain = {min: number; max: number};

/**
 * Widen `held` to cover `column`, or establish it. Never narrows: a pan that happens to land on a
 * narrow view of the data must not recolour everything that is still on screen.
 *
 * Returns `null` for a column with no numeric reading (`bool`, `utf8`) or no marks.
 */
export function widenDomain(held: Domain | null, column: ScalarColumn): Domain | null {
  const values = numericValues(column);
  if (!values || values.length === 0) return held;
  let min = Infinity;
  let max = -Infinity;
  for (let i = 0; i < values.length; i++) {
    const v = values[i]!;
    if (!Number.isFinite(v)) continue;
    if (v < min) min = v;
    if (v > max) max = v;
  }
  if (min === Infinity) return held;
  if (!held) return {min, max};
  return {min: Math.min(held.min, min), max: Math.max(held.max, max)};
}

/**
 * A column's values as plain numbers, or `null` if it has no ramp.
 *
 * `i64` and `timestamp_us` arrive as `BigInt64Array`. Narrowing to a double loses precision past
 * 2⁵³ — but a colour ramp has ~256 distinguishable steps, so the loss is invisible *here* and
 * would be unacceptable anywhere the value is displayed. That is why this is local to colour and
 * the decoder still hands back the `BigInt64Array`.
 */
function numericValues(column: ScalarColumn): ArrayLike<number> | null {
  switch (column.arrowType) {
    case 'bool':
    case 'utf8':
      return null;
    case 'u64':
    case 'i64':
    case 'timestamp_us': {
      const out = new Float64Array(column.values.length);
      for (let i = 0; i < column.values.length; i++) out[i] = Number(column.values[i]!);
      return out;
    }
    default:
      return column.values;
  }
}

/** How a column is being coloured. `uniform` is the default and the fallback for anything unmapped. */
export type Encoding =
  | {kind: 'uniform'}
  /**
   * A column was chosen and its values cannot be named — a refused `/v1/categories`, today the
   * `derived` gate. Distinct from `uniform`, which means no column was chosen: the marks are
   * still all drawn, but the map must show that their value is unknown rather than that no
   * encoding was asked for.
   */
  | {kind: 'unmapped'}
  | {kind: 'category'; column: string; rankOfCode: Ranks}
  | {kind: 'numeric'; column: string; domain: Domain};

/** Palette rank per code, as a plain object so it lives comfortably in the store. */
export type Ranks = Record<number, number>;

/**
 * Count each code's occurrences among the marks on screen.
 *
 * Over the *served* marks, which is a sample of the visible set rather than the set — fine here,
 * because the result decides only which values get a colour. It never becomes a displayed count:
 * a per-value total would be C8's `and_cardinality` against `M_auth`, which is a different
 * question and is deliberately not asked.
 */
/**
 * {@link countCodes}, memoised on the column object.
 *
 * Bands are immutable, so a band's code counts never change — but the legend re-counts the whole
 * frame whenever new codes could have arrived, and scanning every mark of every held band measured
 * 54 ms per count at 10^5 bands. Memoised per column, a recount scans only the bands it has never
 * seen and merges small held maps for the rest: the work becomes proportional to what arrived.
 */
const heldCounts = new WeakMap<object, Map<number, number>>();

export function countCodesCached(column: ScalarColumn): Map<number, number> {
  let held = heldCounts.get(column);
  if (!held) {
    held = countCodes(column);
    heldCounts.set(column, held);
  }
  return held;
}

export function countCodes(column: ScalarColumn): Map<number, number> {
  const counts = new Map<number, number>();
  const values = column.values as ArrayLike<number | bigint>;
  for (let i = 0; i < values.length; i++) {
    const code = Number(values[i]);
    if (code === 0) continue; // *absent* is not a value
    counts.set(code, (counts.get(code) ?? 0) + 1);
  }
  return counts;
}

/**
 * Extend `held` with ranks for any code it does not carry, **most frequent first**.
 *
 * **Frequency, not key order.** A 171-value vocabulary against a 12-colour palette has to choose
 * which values get a colour, and alphabetical order chooses arbitrarily: on the arXiv fixture it
 * spends the whole palette on `acc-phys`…`atom-ph` and greys out every category anyone is looking
 * at. Ranking by what is actually on screen puts the colours where the marks are.
 *
 * **Sticky, for the same reason the numeric domain is.** Ranks already assigned are never
 * reordered, so a pan that changes which value is commonest does not recolour the map underneath
 * the reader. The first viewport establishes the palette; later ones only append. Cleared on
 * principal change, a different mask being a different set of values.
 */
export function extendRanks(held: Ranks, counts: Map<number, number>): Ranks {
  const unranked = [...counts.entries()].filter(([code]) => held[code] === undefined);
  if (unranked.length === 0) return held;
  unranked.sort((a, b) => b[1] - a[1] || a[0] - b[0]);
  const next = {...held};
  let rank = Object.keys(held).length;
  for (const [code] of unranked) next[code] = rank++;
  return next;
}

/** The resolved values that hold a palette colour, in rank order — the legend's list. */
export function paletteValues(
  values: readonly CategoryValue[],
  ranks: Ranks
): {value: CategoryValue; rank: number}[] {
  return values
    .map((value) => ({value, rank: ranks[value.code] ?? Number.MAX_SAFE_INTEGER}))
    .filter((v) => v.rank < PALETTE.length)
    .sort((a, b) => a.rank - b.rank);
}

/** The colour for palette rank `i`, or {@link UNMAPPED} past the palette's end. */
export function colourOfRank(rank: number | undefined): Rgba {
  if (rank === undefined || rank >= PALETTE.length) return UNMAPPED;
  return PALETTE[rank]!;
}

/** The ramp colour for `t` in `[0, 1]`. */
export function colourOfFraction(t: number): Rgba {
  const c = Math.min(1, Math.max(0, t));
  return [
    Math.round(RAMP_LOW[0] + (RAMP_HIGH[0] - RAMP_LOW[0]) * c),
    Math.round(RAMP_LOW[1] + (RAMP_HIGH[1] - RAMP_LOW[1]) * c),
    Math.round(RAMP_LOW[2] + (RAMP_HIGH[2] - RAMP_LOW[2]) * c),
    ALPHA
  ];
}

/**
 * Colour `count` marks into `out` starting at mark `offset`, reading `column` from its own index 0.
 *
 * **Written per band rather than per frame**, which is what lets the slab colour an arriving band
 * without touching the marks already resident. `column` is that band's own values, so the source
 * index and the destination index differ — the reason this takes an offset at all.
 *
 * **Exactly one entry per mark, unconditionally.** Every mark in `[offset, offset + count)` is
 * written whatever the encoding and whatever the data: a value that cannot be resolved contributes
 * a grey mark, never an untouched one. Colour is presentation and must never decide what is drawn,
 * and a gap here is the one way it quietly could — an unwritten span is transparent black.
 */
export function writeColours(
  out: Uint8Array,
  offset: number,
  count: number,
  column: ScalarColumn | undefined,
  encoding: Encoding
): void {
  // **Written as packed `u32`s, one store per mark.** Four byte-writes per mark through a closure
  // measured 23.9 ms per 10^6 marks; a packed table and a `Uint32Array` view measured 1.9 ms — and
  // this runs for every arriving band and every stand-in rebuild, at 10^5–10^6 marks a time. The
  // packing goes through a byte scratch, so it is endian-correct without a byte-order branch.
  const out32 = new Uint32Array(out.buffer, out.byteOffset + offset * 4, count);

  if (encoding.kind === 'uniform' || encoding.kind === 'unmapped') {
    out32.fill(packRgba(encoding.kind === 'uniform' ? UNIFORM : UNMAPPED));
    return;
  }

  if (!column) {
    // The band does not carry this column — a schema change under a live session, say. Grey rather
    // than an exception: the marks are still correct, only their colour is unknown.
    out32.fill(packRgba(UNMAPPED));
    return;
  }

  if (encoding.kind === 'category') {
    const codes = column.values as ArrayLike<number | bigint>;
    // Packed per code on first sight, so the paletteObject-to-u32 work is per distinct code —
    // a handful — rather than per mark. Code 0 is *absent* and never in the rank map, so it falls
    // through to UNMAPPED without its own branch.
    const packed = new Map<number, number>();
    for (let i = 0; i < count; i++) {
      const code = Number(codes[i]);
      let p = packed.get(code);
      if (p === undefined) {
        p = packRgba(colourOfRank(encoding.rankOfCode[code]));
        packed.set(code, p);
      }
      out32[i] = p;
    }
    return;
  }

  const values = numericValues(column);
  if (!values) {
    out32.fill(packRgba(UNMAPPED));
    return;
  }
  const {min, max} = encoding.domain;
  // The ramp quantised to 256 packed steps — the eye cannot use more, and it turns a tuple
  // allocation per mark into a table read. A single-valued domain has no gradient to spread
  // across; every mark sits at the same point of the ramp rather than dividing by zero.
  const span = max - min;
  const ramp = new Uint32Array(256);
  for (let i = 0; i < 256; i++) ramp[i] = packRgba(colourOfFraction(i / 255));
  const unmapped = packRgba(UNMAPPED);
  for (let i = 0; i < count; i++) {
    const v = values[i];
    if (v === undefined || !Number.isFinite(v)) {
      out32[i] = unmapped;
      continue;
    }
    const t = span === 0 ? 0.5 : (v - min) / span;
    out32[i] = ramp[Math.max(0, Math.min(255, Math.round(t * 255)))]!;
  }
}

/** An RGBA tuple as the packed `u32` a `Uint32Array` view stores — endian-correct via the scratch. */
const packScratch = new Uint8Array(4);
const packScratch32 = new Uint32Array(packScratch.buffer);

function packRgba(c: Rgba): number {
  packScratch[0] = c[0];
  packScratch[1] = c[1];
  packScratch[2] = c[2];
  packScratch[3] = c[3];
  return packScratch32[0]!;
}

/** {@link writeColours} over a whole buffer of its own — the provisional layer, rebuilt per frame. */
export function buildColourAttribute(
  pointCount: number,
  scalars: Record<string, ScalarColumn>,
  encoding: Encoding
): Uint8Array {
  const out = new Uint8Array(pointCount * 4);
  const column = encoding.kind === 'category' || encoding.kind === 'numeric'
    ? scalars[encoding.column]
    : undefined;
  writeColours(out, 0, pointCount, column, encoding);
  return out;
}

/** `rgb(...)` for a legend swatch. */
export function css(c: Rgba): string {
  return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
}

/**
 * A column's value at one point, as text — for the item panel, where a category must read as its
 * key rather than as the integer the wire carried.
 */
export function formatScalar(
  column: ScalarColumn,
  index: number,
  keyOfCode: Map<number, string> | null
): string {
  const raw = column.values[index];
  if (raw === undefined) return '—';
  if (keyOfCode) {
    const code = Number(raw);
    if (code === 0) return 'absent';
    return keyOfCode.get(code) ?? `code ${code} (unresolved)`;
  }
  if (column.arrowType === 'timestamp_us') {
    // The unit is in the type rather than in a convention, so it can be rendered as a time.
    return new Date(Number(raw as bigint) / 1000).toISOString();
  }
  return String(raw);
}
