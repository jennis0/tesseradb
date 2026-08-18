import {describe, expect, it} from 'vitest';
import type {Artifact} from '@tessera/client';
import {
  placedArtifacts,
  servedLineage,
  subtreeOf,
  type ArtifactPlaces
} from '../src/artifacts.js';

/**
 * The join between what the service served and where the publisher said it was.
 *
 * It is one-directional, and that is the whole of what these tests hold: the response decides what
 * is drawn, the sidecar only decides *where*. The sidecar is publisher-side knowledge — it knows
 * every cluster, including ones this principal is served none of — so a join that walked it
 * instead would put clusters on screen that the server withheld, which is the disclosure the
 * scaffolding must not be able to cause.
 */

const artifact = (stableKey: string | null, maskedCount: bigint, id = 1n): Artifact => ({
  layer: 'clusters/x',
  tesseraId: id,
  stableKey,
  maskedCount
});

const places: ArtifactPlaces = new Map([
  ['c-0000', {x: 100, y: 200}],
  ['c-0001', {x: 300, y: 400}],
  ['c-0002', {x: 500, y: 600}]
]);

describe('placedArtifacts', () => {
  it('draws only what the response carried, whatever else the sidecar knows about', () => {
    const placed = placedArtifacts([artifact('c-0001', 42n)], places);
    expect(placed.map((p) => p.artifact.stableKey)).toEqual(['c-0001']);
    expect(placed[0]!).toMatchObject({x: 300, y: 400});
  });

  it('drops a served artifact the sidecar cannot place rather than guessing a position', () => {
    // Still listed with its count in the panel — the count came from the service and only the
    // position did not. Inventing one would draw a cluster somewhere it is not.
    expect(placedArtifacts([artifact('c-9999', 42n)], places)).toEqual([]);
    expect(placedArtifacts([artifact(null, 42n)], places)).toEqual([]);
  });

  it('draws the largest count last, so a small cluster is never lost under a large one', () => {
    const placed = placedArtifacts(
      [artifact('c-0002', 900n, 3n), artifact('c-0000', 5n, 1n), artifact('c-0001', 40n, 2n)],
      places
    );
    expect(placed.map((p) => p.artifact.maskedCount)).toEqual([5n, 40n, 900n]);
  });
});

/**
 * The tree, assembled from what one response carried.
 *
 * What these hold is the one rule the wire field has: **a link that does not resolve is no link.**
 * A parent this principal was not served arrives as null, identically to no parent at all, and
 * anything that tried to tell the two apart would be reporting the existence of a coarser artifact
 * the server declined to show.
 */

const node = (id: bigint, parentId: bigint | null, maskedCount = 10n): Artifact => ({
  ...artifact(`k-${id}`, maskedCount, id),
  parentId
});

describe('servedLineage', () => {
  it('nests a child under the parent that was served with it', () => {
    const lineage = servedLineage([node(1n, null), node(2n, 1n), node(3n, 1n)]);
    expect(lineage.roots.map((a) => a.tesseraId)).toEqual([1n]);
    expect(lineage.childrenOf.get(1n)?.map((a) => a.tesseraId)).toEqual([2n, 3n]);
    expect(lineage.linked).toBe(true);
  });

  it('treats a parent that was not served as no parent at all', () => {
    // 7 was withheld — below its own criterion for this principal, suppressed, or dropped by the
    // cut. The response cannot say which, and this must not invent a state for it: 2 is simply a
    // root of what this viewer was given.
    const lineage = servedLineage([node(2n, 7n), node(3n, null)]);
    expect(lineage.roots.map((a) => a.tesseraId)).toEqual([2n, 3n]);
    expect(lineage.linked).toBe(false);
  });

  it('reports a flat response as unlinked rather than as a forest of one-node trees', () => {
    expect(servedLineage([node(1n, null), node(2n, null)]).linked).toBe(false);
  });
});

describe('subtreeOf', () => {
  it('collects an artifact and everything served beneath it', () => {
    const lineage = servedLineage([node(1n, null), node(2n, 1n), node(4n, 2n), node(3n, null)]);
    expect([...subtreeOf(lineage, 1n)].sort()).toEqual([1n, 2n, 4n]);
    expect([...subtreeOf(lineage, 3n)]).toEqual([3n]);
  });

  it('terminates on a response that names a cycle', () => {
    // Not tidiness about a server that would not do this: it is what makes a walk over data from
    // outside the program safe to run inside the frame loop.
    const lineage = servedLineage([node(1n, 2n), node(2n, 1n)]);
    expect(subtreeOf(lineage, 1n).size).toBe(2);
  });
});
