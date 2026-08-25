import {describe, expect, it} from 'vitest';
import {NO_ORDINAL, SessionArtifactTable, type ArtifactRef} from '../src/artifactTable.js';

const ref = (id: bigint, parentId: bigint | null = null, layer = 'clusters/x'): ArtifactRef => ({
  tesseraId: id,
  layer,
  parentId
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

  it('links a child to a parent served in the same batch, and sets its level', () => {
    const table = new SessionArtifactTable();
    const [parent, child] = table.take([ref(1n), ref(2n, 1n)]);
    expect(table.entry(child)?.parentOrdinal).toBe(parent);
    expect(table.entry(child)?.level).toBe(1);
    expect(table.entry(parent)?.level).toBe(0);
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
    const [root, mid, leaf] = table.take([ref(1n), ref(2n, 1n), ref(3n, 2n)]);
    const served = new Set([root!, mid!, leaf!]);
    expect(table.resolve(leaf!, served)).toBe(leaf);
    expect(table.resolve(leaf!, served, 1)).toBe(mid);
    expect(table.resolve(leaf!, served, 0)).toBe(root);
    // A level above everything served resolves to neutral, never to a deeper artifact.
    expect(table.resolve(leaf!, new Set([leaf!]), 0)).toBe(NO_ORDINAL);
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
