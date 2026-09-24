import {describe, expect, it} from 'vitest';
import {LayerManager, type Layer} from '@deck.gl/core';
import {SessionArtifactTable, servedLineage, type ArtifactsProjection, type Band, type MarksProjection} from '@tesseradb/client';
import {LookupTexture, MarkSlab, TesseraLayer, type TesseraLayerProps} from '../src/index.js';
import {fakeDevice, type FakeResource} from './fake-device.js';

function band(tag: number, n: number): Band {
  return {
    depth: 2,
    prefix: BigInt(tag),
    x: tag,
    y: 0,
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(tag * 1000 + i)),
    positions: Float32Array.from({length: n * 2}, (_, i) => tag * 100 + i),
    scalars: {},
    served: n,
    capUsed: 500,
    visible: BigInt(n),
    matched: BigInt(n),
    highlighted: BigInt(n),
    highlightBits: null,
    membership: {},
    heldBelow: BigInt(tag * 1000 + n),
    identityKey: 'ik',
    contentKey: 'ck',
    bytes: n * 32,
    touchedAt: 0
  };
}

const marks = (n: number): MarksProjection => ({bands: [band(1, n)], standIn: [], count: {shown: n, total: n, exact: true}});

/** No artifacts, which is enough for the layer to build its lookup texture. */
function artifacts(): ArtifactsProjection {
  return {
    layer: null,
    layers: [],
    served: [],
    colourServed: [],
    lineage: servedLineage([]),
    status: 'idle',
    refusal: null,
    version: 0,
    held: 0,
    table: new SessionArtifactTable(),
    servedOrdinals: new Set(),
    shapes: new Map(),
    colours: new Map(),
    palette: 'positional',
    coverage: {current: 0, stale: 0}
  };
}

/**
 * The layer under deck.gl's own `LayerManager`, the object a `Deck` drives it through: matching by
 * id, state carried to each new instance, finalised when it leaves the list. The fake device draws
 * nothing, so the sublayers fail to build their programs; only the Tessera layer's own errors count.
 * What the layer drew with is read off its first marks sublayer: the positions buffer it bound and
 * the lookup texture it passed.
 */
function host() {
  const device = fakeDevice();
  const manager = new LayerManager(device, {});
  const errors: unknown[] = [];
  manager.setProps({onError: (error: unknown, layer: Layer) => (layer.id === 'tessera' ? errors.push(error) : undefined)});
  const marksLayer = () => manager.getLayers().find((l) => l.id === 'tessera-marks-p0');
  return {
    device,
    errors,
    draw: (props: Partial<TesseraLayerProps>) =>
      manager.setLayers([new TesseraLayer({id: 'tessera', depth: 2, status: 'shown', artifacts: artifacts(), ...props} as TesseraLayerProps)]),
    remove: () => manager.setLayers([]),
    buffer: () => {
      const data = marksLayer()?.props.data as {attributes: Record<string, {buffer?: FakeResource}>} | undefined;
      return data?.attributes['instancePositions']?.buffer ?? null;
    },
    texture: () => ((marksLayer()?.props as {lutTexture?: FakeResource | null} | undefined)?.lutTexture ?? null)
  };
}

describe('TesseraLayer GPU resources', () => {
  it('makes its own slab and lookup texture on its device when the host passes none', () => {
    const h = host();
    h.draw({marks: marks(3)});
    expect(h.errors).toEqual([]);
    expect(h.buffer()).toMatchObject({destroyed: false});
    expect(h.texture()).toMatchObject({destroyed: false});
  });

  it('keeps them across prop updates', () => {
    const h = host();
    h.draw({marks: marks(3)});
    const buffer = h.buffer();
    const texture = h.texture();
    h.draw({marks: marks(3), scheme: 'light', labels: false});
    h.draw({marks: marks(3), radius: 4});
    expect(h.errors).toEqual([]);
    expect(h.buffer()).toBe(buffer);
    expect(h.texture()).toBe(texture);
    expect(buffer!.destroyed || texture!.destroyed).toBe(false);
  });

  it('releases them when it is finalised', () => {
    const h = host();
    h.draw({marks: marks(3)});
    const buffer = h.buffer();
    const texture = h.texture();
    h.remove();
    expect(h.errors).toEqual([]);
    expect(buffer!.destroyed).toBe(true);
    expect(texture!.destroyed).toBe(true);
  });

  it('draws through the slab and lookup texture the host passes and never releases them', () => {
    const h = host();
    const slab = new MarkSlab();
    const lut = new LookupTexture();
    slab.attach(h.device);
    lut.attach(h.device);
    h.draw({marks: marks(3), slab, lut});
    h.draw({marks: marks(3), slab, lut, scheme: 'light'});
    expect(h.errors).toEqual([]);
    const buffer = slab.layers()[0]!.draw.gpu!.positions as unknown as FakeResource;
    expect(h.buffer()).toBe(buffer);
    expect(h.texture()).toBe(lut.gpu);
    h.remove();
    expect(buffer.destroyed).toBe(false);
    expect((lut.gpu as unknown as FakeResource).destroyed).toBe(false);
    expect(slab.drawn).toBe(3);
  });

  it('releases its own slab when the host starts passing one', () => {
    const h = host();
    h.draw({marks: marks(3)});
    const own = h.buffer();
    const slab = new MarkSlab();
    slab.attach(h.device);
    h.draw({marks: marks(3), slab});
    expect(h.errors).toEqual([]);
    expect(own!.destroyed).toBe(true);
    expect(h.buffer()).toBe(slab.layers()[0]!.draw.gpu!.positions);
  });
});
