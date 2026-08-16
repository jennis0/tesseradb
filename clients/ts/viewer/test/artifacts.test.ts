import {describe, expect, it} from 'vitest';
import type {Artifact} from '@tessera/client';
import {placedArtifacts, type ArtifactPlaces} from '../src/artifacts.js';

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
