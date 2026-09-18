import {describe, expect, it} from 'vitest';
import {NEUTRAL, SessionArtifactTable, artifactColours, positionalEntry, type Artifact, type Band, type ScalarColumn} from '@tesseradb/client';
import {LookupTexture, LUT_WIDTH, buildLut, dimmed} from '../src/lut.js';
import {MarkSlab} from '../src/slab.js';
import {fakeDevice} from './fake-device.js';

const artifact = (id: bigint, x: number, parent: bigint | null = null): Artifact => ({
  layer: 'l',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 10n,
  centroid: [x, 2 ** 31],
  box: null,
  shape: null,
  content: [],
  parentIds: parent === null ? [] : [parent],
  rung: 0,
  matched: null,
  highlighted: null,
  target: null
});

function band(tag: number, ordinals: number[], layer = 'l'): Band {
  const n = ordinals.length;
  const scalars: Record<string, ScalarColumn> = {c: {arrowType: 'u32', values: Uint32Array.from({length: n}, () => tag)}};
  const o = Uint32Array.from(ordinals);
  return {
    depth: 2,
    prefix: BigInt(tag),
    x: tag,
    y: 0,
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(tag * 1000 + i)),
    positions: Float32Array.from({length: n * 2}, (_, i) => tag * 100 + i),
    scalars,
    membership: {[layer]: {ordinals: o, distinct: Uint32Array.from(new Set(ordinals.filter((v) => v !== 0)))}},
    served: n,
    capUsed: 500,
    visible: BigInt(n),
    matched: BigInt(n),
    highlighted: BigInt(n),
    highlightBits: null,
    heldBelow: BigInt(tag * 1000 + n),
    identityKey: 'ik',
    contentKey: 'ck',
    bytes: n * 36,
    touchedAt: 0
  };
}

/** A table naming a root and two children, with every ordinal served. */
function served() {
  const table = new SessionArtifactTable();
  // Rungs are the wire's, so a reference states one: the two children are served at rung 1,
  // which is what the level walk below resolves against.
  const [root, a, b] = table.take([
    {tesseraId: 1n, layer: 'l', parentIds: [], rung: 0},
    {tesseraId: 2n, layer: 'l', parentIds: [1n], rung: 1},
    {tesseraId: 3n, layer: 'l', parentIds: [1n], rung: 1}
  ]);
  const arts = [artifact(1n, 2 ** 31 + 100), artifact(2n, 2 ** 31 + 1e9, 1n), artifact(3n, 2 ** 31 - 1e9, 1n)];
  const named = [root!, a!, b!].map((ordinal, i) => ({ordinal, centroid: arts[i]!.centroid}));
  return {table, root: root!, a: a!, b: b!, arts, named};
}

describe('buildLut', () => {
  it('colours each ordinal by what it resolves to, neutral for 0 and for a freed slot', () => {
    const {table, root, a, named} = served();
    const colours = artifactColours(named, 'positional');
    const lut = buildLut({artifacts: {table, colours}});
    expect(lut.rows).toBe(1);
    expect(lut.data.length).toBe(LUT_WIDTH * 4);
    expect([...lut.data.subarray(0, 4)]).toEqual([...NEUTRAL]);
    expect([...lut.data.subarray(a * 4, a * 4 + 4)]).toEqual([...colours.get(a)!]);
    // The level walk: at level 0 the child resolves to the root's colour.
    const atRoot = buildLut({artifacts: {table, colours}, level: 0});
    expect([...atRoot.data.subarray(a * 4, a * 4 + 4)]).toEqual([...colours.get(root)!]);
    // Nothing colourable: every ordinal neutral.
    const none = buildLut({artifacts: {table, colours: new Map()}});
    expect([...none.data.subarray(a * 4, a * 4 + 4)]).toEqual([...NEUTRAL]);
  });

  it('colours an ordinal the current view was not served, and walks up to one it has a colour for', () => {
    // The banding on a zoom in: the cut moves finer, the channel's served set moves with it, and
    // every band held under the coarser cut named artifacts no longer in it. A walk cannot go
    // down, so those points drew neutral until the tile was refetched.
    const {table, root, a, named} = served();
    // The colour map is the whole table's, so the child is coloured though only the root is in
    // the view's served set.
    const whole = artifactColours(named, 'positional');
    expect([...buildLut({artifacts: {table, colours: whole}}).data.subarray(a * 4, a * 4 + 4)]).toEqual([...whole.get(a)!]);

    // And where a colour genuinely is not known for the ordinal — an artifact evicted from the
    // table's colour map — the walk goes *up* to the parent that is, not to neutral.
    const parentOnly = new Map(whole);
    parentOnly.delete(a);
    expect([...buildLut({artifacts: {table, colours: parentOnly}}).data.subarray(a * 4, a * 4 + 4)]).toEqual([...whole.get(root)!]);
  });

  it('colours a node with two parents through the first of them — the wire’s lowest id (decision 0117)', () => {
    const table = new SessionArtifactTable();
    const [a, b, child] = table.take([
      {tesseraId: 1n, layer: 'l', parentIds: [], rung: 0},
      {tesseraId: 2n, layer: 'l', parentIds: [], rung: 0},
      {tesseraId: 3n, layer: 'l', parentIds: [1n, 2n], rung: 1}
    ]);
    const arts = [artifact(1n, 2 ** 31 + 1e9), artifact(2n, 2 ** 31 - 1e9), {...artifact(3n, 2 ** 31), parentIds: [1n, 2n]}];
    const colours = artifactColours([a!, b!, child!].map((ordinal, i) => ({ordinal, centroid: arts[i]!.centroid})), 'positional');
    expect([...colours.get(a!)!]).not.toEqual([...colours.get(b!)!]);
    // At its own rung the child wears its own colour; coarsened to rung 0 it wears the first
    // parent's, and the second parent's colour is never what the walk lands on.
    expect([...buildLut({artifacts: {table, colours}}).data.subarray(child! * 4, child! * 4 + 4)]).toEqual([...colours.get(child!)!]);
    expect([...buildLut({artifacts: {table, colours}, level: 0}).data.subarray(child! * 4, child! * 4 + 4)]).toEqual([...colours.get(a!)!]);
  });

  it('highlights the opened artifact and dims the rest', () => {
    const {table, a, b, named} = served();
    const colours = artifactColours(named, 'positional');
    const lut = buildLut({artifacts: {table, colours}, highlight: a});
    expect([...lut.data.subarray(a * 4, a * 4 + 4)]).toEqual([...colours.get(a)!]);
    expect([...lut.data.subarray(b * 4, b * 4 + 4)]).toEqual([...dimmed(colours.get(b)!)]);
    expect(lut.data[b * 4 + 3]).toBeLessThan(colours.get(b)![3]);
  });

  it('grows in rows of a power of two with the table range', () => {
    const table = new SessionArtifactTable();
    table.take(Array.from({length: 3000}, (_, i) => ({tesseraId: BigInt(i + 1), layer: 'l', parentIds: []})));
    const lut = buildLut({artifacts: {table, colours: new Map()}});
    expect(lut.rows).toBe(4);
  });
});

/**
 * The rewrite is bounded by what changed, not by what the session table holds: after the artifact
 * channel's idle promotion the table holds whole levels — 226k entries on GeoNames — while a
 * settled view names a few hundred it had not seen.
 */
describe('a table that only gained ordinals patches the rows they fall in', () => {
  /** The colour of one ordinal, as the texture holds it. */
  const texel = (lut: LookupTexture, o: number) => [...lut.colourOf(o)];

  it('writes the new ordinals’ rows and no others, and leaves every other texel as it was', () => {
    const device = fakeDevice();
    const lut = new LookupTexture();
    lut.attach(device);
    const table = new SessionArtifactTable();
    // A table wide enough to span rows: the ordinals named next land at the top of the range.
    table.take(Array.from({length: 2000}, (_, i) => ({tesseraId: BigInt(i + 1), layer: 'l', parentIds: [], centroid: [2 ** 31 + i, 2 ** 31] as [number, number]})));
    const colours = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      'positional'
    );
    lut.update({artifacts: {table, colours}}, 'k');
    expect(device.textureWrites).toBe(1);
    expect(device.textureRegions).toEqual([{y: 0, height: 2}]);
    const settled = texel(lut, 7);

    // One artifact named, its colour added to the map the store extends in place.
    const [fresh] = table.take([{tesseraId: 9001n, layer: 'l', parentIds: [], centroid: [2 ** 31 - 5e8, 2 ** 31 + 5e8]}]);
    colours.set(fresh!, positionalEntry(table.entry(fresh!)!.centroid));
    expect(lut.update({artifacts: {table, colours}}, 'k')).toBe(true);
    expect(device.textureWrites).toBe(2);
    // Row 1 alone — the row the new ordinal falls in — and not the two the texture holds.
    expect(device.textureRegions[1]).toEqual({y: fresh! >> 10, height: 1});
    expect(texel(lut, fresh!)).toEqual([...colours.get(fresh!)!]);
    expect(texel(lut, 7)).toEqual(settled);

    // And the same table again writes nothing.
    expect(lut.update({artifacts: {table, colours}}, 'k')).toBe(false);
    expect(device.textureWrites).toBe(2);
  });

  it('rebuilds whole where a change can move a texel that is not its own', () => {
    const device = fakeDevice();
    const lut = new LookupTexture();
    lut.attach(device);
    // Two rows of entries, so a whole rebuild and a patch are told apart by the region written.
    const table = new SessionArtifactTable();
    table.take(Array.from({length: 2000}, (_, i) => ({tesseraId: BigInt(i + 1), layer: 'l', parentIds: [], centroid: [2 ** 31 + i, 2 ** 31] as [number, number]})));
    const parent = table.ordinalOf('l', 1n);
    const [child] = table.take([{tesseraId: 9001n, layer: 'l', parentIds: [1n], rung: 1, centroid: [2 ** 31, 2 ** 31 + 1e9]}]);
    const colours = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      'positional'
    );
    lut.update({artifacts: {table, colours}, level: 0}, 'k');
    expect(device.textureRegions).toEqual([{y: 0, height: 2}]);
    // Coloured at level 0, the child wears the parent's colour.
    expect([...lut.colourOf(child!)]).toEqual([...colours.get(parent)!]);

    // The parent freed: what resolved *through* it moves, and that is not its own texel, so the
    // texture is rebuilt whole rather than patched at the freed ordinal's row.
    table.release([parent]);
    colours.delete(parent);
    expect(lut.update({artifacts: {table, colours}, level: 0}, 'k')).toBe(true);
    expect(device.textureRegions[1]).toEqual({y: 0, height: 2});
    expect([...lut.colourOf(child!)]).toEqual([...NEUTRAL]);
  });

  it('keeps the coverage rule: an ordinal the current view was not served is still coloured through a patch', () => {
    // The point frame names an artifact ahead of the debounced channel. The walk stops at what is
    // colourable, not at what the view was served, and a patched rewrite must not narrow that.
    const device = fakeDevice();
    const lut = new LookupTexture();
    lut.attach(device);
    const table = new SessionArtifactTable();
    table.take(Array.from({length: 2000}, (_, i) => ({tesseraId: BigInt(i + 1), layer: 'l', parentIds: [], centroid: [2 ** 31 + i, 2 ** 31] as [number, number]})));
    const parent = table.ordinalOf('l', 1n);
    const colours = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      'positional'
    );
    lut.update({artifacts: {table, colours}}, 'k');

    // Named by the point frame alone — no colour of its own — under a parent already held.
    const [late] = table.take([{tesseraId: 9001n, layer: 'l', parentIds: [1n], rung: 1}]);
    expect(lut.update({artifacts: {table, colours}}, 'k')).toBe(true);
    // Patched — its row alone — and coloured by the ancestor the walk reaches, not neutral.
    expect(device.textureRegions[1]).toEqual({y: late! >> 10, height: 1});
    expect([...lut.colourOf(late!)]).toEqual([...colours.get(parent)!]);
  });
});

describe('an ordinal named before its colour exists', () => {
  it('is written neutral, then coloured on the update after the map is extended in place', () => {
    const device = fakeDevice();
    const lut = new LookupTexture();
    lut.attach(device);
    const table = new SessionArtifactTable();
    table.take(Array.from({length: 50}, (_, i) => ({tesseraId: BigInt(i + 1), layer: 'l', parentIds: [], centroid: [2 ** 31 + i, 2 ** 31] as [number, number]})));
    const colours = artifactColours(table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})), 'positional');
    lut.update({artifacts: {table, colours}}, 'k');
    // A points frame names an ordinal and the layer draws before the store has coloured it.
    const [fresh] = table.take([{tesseraId: 9001n, layer: 'l', parentIds: [], centroid: [2 ** 31 - 5e8, 2 ** 31 + 5e8]}]);
    lut.update({artifacts: {table, colours}}, 'k');
    expect([...lut.colourOf(fresh!)]).toEqual([...NEUTRAL]);
    const writes = device.textureWrites;
    // Nothing changed: no write.
    expect(lut.update({artifacts: {table, colours}}, 'k')).toBe(false);
    expect(device.textureWrites).toBe(writes);
    // The store extends the same map in place; the next draw colours the ordinal, one row written.
    colours.set(fresh!, positionalEntry(table.entry(fresh!)!.centroid));
    expect(lut.update({artifacts: {table, colours}}, 'k')).toBe(true);
    expect(device.textureWrites).toBe(writes + 1);
    expect([...lut.colourOf(fresh!)]).toEqual([...colours.get(fresh!)!]);
    // And settled means settled: the same inputs again write nothing.
    expect(lut.update({artifacts: {table, colours}}, 'k')).toBe(false);
  });
});

describe('every colouring interaction is a texture rewrite, never an attribute upload (decision 0100)', () => {
  it('palette, level, highlight and the switch write the texture and not the buffers', () => {
    const {table, a, b, root, named} = served();
    const device = fakeDevice();
    const slab = new MarkSlab();
    slab.attach(device);
    const lut = new LookupTexture();
    lut.attach(device);

    // Two bands land: positions, colours and ordinals upload — the ordinary write path.
    const bands = [band(1, [a, a, 0]), band(2, [b, root])];
    slab.sync(bands, 2, {kind: 'uniform'}, null, 'l');
    const uploadsAfterBands = device.bufferWrites;
    expect(uploadsAfterBands).toBeGreaterThan(0);
    // The colour map is held per palette, as the store holds it: `update` compares it by
    // identity, so a fresh map of the same colours is a recolour and rewrites (see below).
    const maps = {positional: artifactColours(named, 'positional'), spread: artifactColours(named, 'spread')};
    const inputs = (palette: 'positional' | 'spread', level?: number, highlight?: number) => ({
      artifacts: {table, colours: maps[palette]},
      level,
      highlight
    });
    expect(lut.update(inputs('positional'), 'v1|positional||')).toBe(true);
    expect(device.textureWrites).toBe(1);

    // A palette change.
    expect(lut.update(inputs('spread'), 'v1|spread||')).toBe(true);
    // A level choice.
    expect(lut.update(inputs('spread', 0), 'v1|spread|0|')).toBe(true);
    // Highlight the opened artifact, dim the rest.
    expect(lut.update(inputs('spread', 0, a), `v1|spread|0|${a}`)).toBe(true);
    expect(device.textureWrites).toBe(4);
    // The same inputs again: nothing is written.
    expect(lut.update(inputs('spread', 0, a), `v1|spread|0|${a}`)).toBe(false);
    expect(device.textureWrites).toBe(4);
    // A different colour map object under the same key is a recolour — the palette or the ground
    // moved — and rewrites though its contents happen to match: the store extends a map in place
    // while the palette holds, so a new object is the signal that every colour may have moved.
    expect(lut.update({artifacts: {table, colours: artifactColours(named, 'spread')}, level: 0, highlight: a}, `v1|spread|0|${a}`)).toBe(true);
    expect(device.textureWrites).toBe(5);

    // The switch between cluster and column colour is a uniform: the slab is asked for the same
    // column encoding with the same carried layer, and uploads nothing.
    slab.sync(bands, 2, {kind: 'uniform'}, null, 'l');
    expect(device.bufferWrites).toBe(uploadsAfterBands);
    expect(lut.writes).toBe(5);
  });

  it('the ordinal attribute uploads through the slab’s dirty span, once per band, and reads back the band’s ordinals', () => {
    const device = fakeDevice();
    const slab = new MarkSlab();
    slab.attach(device);
    const draw = slab.sync([band(1, [5, 0, 7])], 2, {kind: 'uniform'}, null, 'l');
    expect([...draw.ordinals]).toEqual([5, 0, 7]);
    // Buffers are created in the order positions, colours, picking, ordinals; the ordinals buffer
    // took exactly one write for the band.
    expect(device.writesByBuffer[3]).toBe(1);
    // A band without the carried layer's column reads as no artifact.
    const next = slab.sync([band(1, [5, 0, 7]), band(2, [9], 'other')], 2, {kind: 'uniform'}, null, 'l');
    expect([...next.ordinals]).toEqual([5, 0, 7, 0]);
    // Switching the carried layer rewrites the ordinals from the columns the bands already hold.
    const switched = slab.sync([band(1, [5, 0, 7]), band(2, [9], 'other')], 2, {kind: 'uniform'}, null, 'other');
    expect([...switched.ordinals]).toEqual([0, 0, 0, 9]);
  });
});
