import {describe, expect, it} from 'vitest';
import {NO_ORDINAL, SessionArtifactTable, type ArtifactRef} from '../src/artifactTable.js';

const ref = (
  id: bigint,
  parentId: bigint | null = null,
  layer = 'clusters/x',
  rung = 0
): ArtifactRef => ({
  tesseraId: id,
  layer,
  parentId,
  rung
});

describe('the session artifact table', () => {
  it('names each artifact once and reserves ordinal 0 for none', () => {
    const table = new SessionArtifactTable();
    const [a, b] = table.take([ref(10n), ref(20n)]);
    expect(a).toBeGreaterThan(NO_ORDINAL);
    expect(b).toBeGreaterThan(NO_ORDINAL);
    expect(a).not.toBe(b);
    // The same id in a later response resolves to the same ordinal — ids are stable per session.
    expect(table.ordinalOf('clusters/x', 10n)).toBe(a);
    expect([...table.take([ref(10n)])]).toEqual([a]);
    expect(table.entry(a)?.tesseraId).toBe(10n);
  });

  it('links a child to a parent served in the same batch, and takes each rung from the wire', () => {
    const table = new SessionArtifactTable();
    const [parent, child] = table.take([ref(1n), ref(2n, 1n, 'clusters/x', 1)]);
    expect(table.entry(child)?.parentOrdinal).toBe(parent);
    expect(table.entry(child)?.rung).toBe(1);
    expect(table.entry(parent)?.rung).toBe(0);
  });

  /**
   * **The rung is the wire's and never the chain's** (contracts §3.2 r43). A tiered layer's edge
   * may skip a level — a city directly under a country because that country has no states — so a
   * child one link below a root is served at rung 2. Counting links said 1 and drew it with the
   * wrong siblings; the count no longer exists here to disagree.
   */
  it('keeps a rung that the parent chain would have disagreed with', () => {
    const table = new SessionArtifactTable();
    const [country, county] = table.take([
      ref(1n, null, 'clusters/x', 0),
      ref(2n, 1n, 'clusters/x', 2)
    ]);
    expect(table.entry(county)?.parentOrdinal).toBe(country);
    expect(table.entry(county)?.rung).toBe(2);
  });

  it('leaves a child a root when its parent is not in the batch — a link that does not resolve is no link', () => {
    const table = new SessionArtifactTable();
    const [child] = table.take([ref(2n, 7n)]);
    expect(table.entry(child)?.parentOrdinal).toBeNull();
  });

  it('refcounts by band and recycles an ordinal at zero', () => {
    const table = new SessionArtifactTable();
    const first = table.take([ref(10n), ref(20n)]); // ref count 1 each
    const ordinal10 = first[0]!;
    table.take([ref(10n)]); // 10 now held by two references
    expect(table.live).toBe(2);

    // Release the first band's references: 10 still held (count 1), 20 drops to zero and recycles.
    table.release(first);
    expect(table.live).toBe(1);
    expect(table.ordinalOf('clusters/x', 20n)).toBe(NO_ORDINAL);
    expect(table.ordinalOf('clusters/x', 10n)).toBe(ordinal10);

    // A new artifact reuses the freed slot rather than growing the range.
    const rangeBefore = table.range;
    const [reused] = table.take([ref(30n)]);
    expect(reused).toBe(first[1]); // the slot 20 vacated
    expect(table.range).toBe(rangeBefore);
  });

  it('resolves an ordinal up to the nearest served ancestor, or neutral where the walk fails', () => {
    const table = new SessionArtifactTable();
    const [root, mid, leaf] = table.take([ref(1n), ref(2n, 1n), ref(3n, 2n)]);
    // The cut serves only the root: a leaf resolves up to it.
    expect(table.resolve(leaf!, new Set([root!]))).toBe(root);
    // The cut serves the mid level: the leaf resolves there instead.
    expect(table.resolve(leaf!, new Set([mid!, root!]))).toBe(mid);
    // Nothing on the path is served: neutral.
    expect(table.resolve(leaf!, new Set())).toBe(NO_ORDINAL);
  });

  it('terminates a resolve over a response that named a cycle', () => {
    const table = new SessionArtifactTable();
    const [a, b] = table.take([ref(1n, 2n), ref(2n, 1n)]);
    // Neither is served: the walk must end, not spin.
    expect(table.resolve(a!, new Set())).toBe(NO_ORDINAL);
    expect(b).toBeGreaterThan(NO_ORDINAL);
  });

  it('drops everything on clear — a new identity key may not reuse a name', () => {
    const table = new SessionArtifactTable();
    table.take([ref(10n)]);
    table.clear();
    expect(table.live).toBe(0);
    expect(table.ordinalOf('clusters/x', 10n)).toBe(NO_ORDINAL);
  });
});

describe('the level walk and retained references', () => {
  it('resolves to the first served ancestor at or above the chosen level', () => {
    const table = new SessionArtifactTable();
    const [root, mid, leaf] = table.take([
      ref(1n, null, 'clusters/x', 0),
      ref(2n, 1n, 'clusters/x', 1),
      ref(3n, 2n, 'clusters/x', 2)
    ]);
    const served = new Set([root!, mid!, leaf!]);
    expect(table.resolve(leaf!, served)).toBe(leaf);
    expect(table.resolve(leaf!, served, 1)).toBe(mid);
    expect(table.resolve(leaf!, served, 0)).toBe(root);
    // A level above everything served resolves to neutral, never to a deeper artifact.
    expect(table.resolve(leaf!, new Set([leaf!]), 0)).toBe(NO_ORDINAL);
  });

  /**
   * **A treed layer, as the wire now gives it** (contracts §3.2 r43): every artifact declared at
   * level 0, and `rung` the response-local chain depth the server computed after the cut. The
   * table used to count that depth from the links itself and pick between the count and the
   * declared level per layer kind (trap 5.4); it now records the wire's number, and the walk
   * coarsens by it — so a treed layer still offers the rungs it has and the walk still coarsens.
   */
  it('coarsens a treed layer by the wire’s rungs, which are its chain depths', () => {
    const table = new SessionArtifactTable();
    const [root, mid, leaf] = table.take([
      ref(1n, null, 'clusters/tree', 0),
      ref(2n, 1n, 'clusters/tree', 1),
      ref(3n, 2n, 'clusters/tree', 2)
    ]);
    expect([root, mid, leaf].map((o) => table.entry(o!)!.rung)).toEqual([0, 1, 2]);
    const served = new Set([root!, mid!, leaf!]);
    expect(table.resolve(leaf!, served, 1)).toBe(mid);
    expect(table.resolve(leaf!, served, 0)).toBe(root);
  });

  /**
   * **A levelled layer whose edge skips a rung** — the case where a chain count is the wrong
   * answer, and the reason the count no longer exists here: the wire's `rung` is the declared
   * level, and a child one link below a root is at 2 because that is what was declared.
   */
  it('coarsens a levelled layer by the declared level, never by a link count', () => {
    const table = new SessionArtifactTable();
    const [country, county] = table.take([
      ref(1n, null, 'admin/boundaries', 0),
      ref(2n, 1n, 'admin/boundaries', 2)
    ]);
    expect(table.entry(county!)!.parentOrdinal).toBe(country);
    expect(table.entry(county!)!.rung).toBe(2);
    // Coarsening to rung 1 passes the county and stops at the country, which is at 0.
    expect(table.resolve(county!, new Set([country!, county!]), 1)).toBe(country);
  });

  it('retain adds references a release must match before an ordinal recycles', () => {
    const table = new SessionArtifactTable();
    const [a] = table.take([ref(10n)]);
    table.retain([a!, NO_ORDINAL, 999]);
    table.release([a!]);
    expect(table.ordinalOf('clusters/x', 10n)).toBe(a);
    table.release([a!]);
    expect(table.ordinalOf('clusters/x', 10n)).toBe(NO_ORDINAL);
  });
});

describe('the table is what a colour is built from (§5.10)', () => {
  it('carries geometry, lists what is live, and stamps a version when it changes', () => {
    const table = new SessionArtifactTable();
    const before = table.version;
    const [a, b] = table.take([
      {tesseraId: 1n, layer: 'l', parentId: null, centroid: [10, 20]},
      {tesseraId: 2n, layer: 'l', parentId: 1n}
    ]);
    expect(table.version).toBeGreaterThan(before);
    expect(table.entry(a!)!.centroid).toEqual([10, 20]);
    // An artifact first named by a frame that declared no centroid takes one when a later frame
    // does — geometry arriving late is a colour arriving late, not a second identity.
    expect(table.entry(b!)!.centroid).toBeNull();
    const named = table.version;
    table.take([{tesseraId: 2n, layer: 'l', parentId: 1n, centroid: [30, 40]}]);
    expect(table.entry(b!)!.centroid).toEqual([30, 40]);
    expect(table.version).toBeGreaterThan(named);
    expect(table.entry(b!)!.parentOrdinal).toBe(a);

    expect(table.liveEntries().map((e) => e.ordinal).sort()).toEqual([a, b].sort());
    // Naming what is already named moves nothing.
    const settled = table.version;
    table.take([{tesseraId: 1n, layer: 'l', parentId: null, centroid: [10, 20]}]);
    expect(table.version).toBe(settled);

    // A freed ordinal leaves the live list and stamps the version.
    table.release([a!, a!, a!]);
    expect(table.liveEntries().map((e) => e.ordinal)).toEqual([b]);
    expect(table.version).toBeGreaterThan(settled);
  });
});
