import {describe, expect, it} from 'vitest';
import {NEUTRAL, SessionArtifactTable, artifactColours, type Artifact, type Band, type ScalarColumn} from '@tesseradb/client';
import {LookupTexture, LUT_WIDTH, buildLut, dimmed} from '../src/lut.js';
import {MarkSlab} from '../src/slab.js';
import {fakeDevice} from './fake-device.js';

const artifact = (id: bigint, x: number, parentId: bigint | null = null): Artifact => ({
  layer: 'l',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 10n,
  centroid: [x, 2 ** 31],
  box: null,
  hull: null,
  content: [],
  parentId,
  level: 0
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
  // Levels are the wire's, so a reference states one: the two children are declared at level 1,
  // which is what the level walk below resolves against.
  const [root, a, b] = table.take([
    {tesseraId: 1n, layer: 'l', parentId: null, level: 0},
    {tesseraId: 2n, layer: 'l', parentId: 1n, level: 1},
    {tesseraId: 3n, layer: 'l', parentId: 1n, level: 1}
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
    table.take(Array.from({length: 3000}, (_, i) => ({tesseraId: BigInt(i + 1), layer: 'l', parentId: null})));
    const lut = buildLut({artifacts: {table, colours: new Map()}});
    expect(lut.rows).toBe(4);
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
    const inputs = (palette: 'positional' | 'spread', level?: number, highlight?: number) => ({
      artifacts: {table, colours: artifactColours(named, palette)},
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

    // The switch between cluster and column colour is a uniform: the slab is asked for the same
    // column encoding with the same carried layer, and uploads nothing.
    slab.sync(bands, 2, {kind: 'uniform'}, null, 'l');
    expect(device.bufferWrites).toBe(uploadsAfterBands);
    expect(lut.writes).toBe(4);
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
