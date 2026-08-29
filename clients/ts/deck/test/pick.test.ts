import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection, type Band} from '@tesseradb/client';
import {artifactOfMark} from '../src/pick.js';

/** A hovered mark names the artifact it is a member of, through its ordinal and the table. */

const artifact = (id: bigint, parentId: bigint | null, rung = 0): Artifact => ({layer: 'clusters', tesseraId: id, key: `c-${id}`, maskedCount: 1n, centroid: null, box: null, shape: null, content: [], parentId, rung, matched: null});

function projection(served: Artifact[]): ArtifactsProjection {
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentId: a.parentId, rung: a.rung})));
  return {layer: 'clusters', layers: ['clusters'], served, lineage: servedLineage(served), status: 'shown', refusal: null, version: 1, held: 0, table, servedOrdinals: new Set(ordinals), shapes: new Map(), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}};
}

const bandWith = (membership: Band['membership']) => ({membership}) as unknown as Band;

describe('artifactOfMark', () => {
  it('resolves the mark’s ordinal to the served artifact, up the tree to the chosen level', () => {
    const p = projection([artifact(1n, null, 0), artifact(2n, 1n, 1)]);
    const child = p.table.ordinalOf('clusters', 2n);
    const band = bandWith({clusters: {ordinals: new Uint32Array([0, child]), distinct: new Uint32Array([child])}});
    expect(artifactOfMark(band, 1, p)).toBe(2n);
    expect(artifactOfMark(band, 1, p, 0)).toBe(1n);
    // Under no artifact, or with no column for the layer on: nothing to outline.
    expect(artifactOfMark(band, 0, p)).toBeNull();
    expect(artifactOfMark(bandWith({}), 1, p)).toBeNull();
    expect(artifactOfMark(band, 1, {...p, layers: []})).toBeNull();
  });
});
