import {afterEach, describe, expect, it, vi} from 'vitest';
import {LayerManager, type Layer} from '@deck.gl/core';
import {WORLD_SIZE, type ArtifactsProjection, type MarksProjection} from '@mosaica/client';
import {SessionArtifactTable, servedLineage} from '@mosaica/client/internal';
import {band} from '../../core/test/support.js';
import {binDensity, filterDensity, type DensityCounts} from '../src/density.js';
import {MosaicaLayer, type MosaicaLayerInternalProps} from '../src/layer.js';
import {MarkSlab} from '../src/slab.js';
import {fakeDevice, type FakeResource} from './fake-device.js';

const marks = (n: number): MarksProjection => ({bands: [band(2, 1n, n)], standIn: [], count: {shown: n, total: n, exact: true}});

/** No artifacts, which is enough for the layer to build its lookup texture. */
function artifacts(): ArtifactsProjection {
  return {
    layer: null,
    layers: [],
    served: [],
    colourServed: [],
    attached: new Map(),
    lineage: servedLineage([]),
    status: 'idle',
    refusal: null,
    version: 0,
    held: 0,
    table: new SessionArtifactTable(),
    servedOrdinals: new Set(),
    shapes: new Map(),
    colours: new Map(),
    palette: 'tableau10', overrides: new Map(),
    coverage: {current: 0, stale: 0}
  };
}

/**
 * The layer under deck.gl's own `LayerManager`, the object a `Deck` drives it through: matching by
 * id, state carried to each new instance, finalised when it leaves the list. The fake device draws
 * nothing, so the sublayers fail to build their programs; only the Mosaica layer's own errors count.
 * What the layer drew with is read off the sublayers it rendered: the positions buffer and lookup
 * texture its first marks sublayer was given, and the wash's bounds.
 */
function host() {
  const device = fakeDevice();
  const manager = new LayerManager(device, {});
  const errors: unknown[] = [];
  manager.setProps({onError: (error: unknown, layer: Layer) => (layer.id === 'mosaica' ? errors.push(error) : undefined)});
  const sublayer = (id: string) => {
    const layer = manager.getLayers().find((l) => l.id === 'mosaica') as MosaicaLayer | undefined;
    return (layer?.getSubLayers() as Layer[] | undefined)?.find((l) => l.id === `mosaica-${id}`);
  };
  const marksLayer = () => sublayer('marks-p0');
  return {
    device,
    errors,
    draw: (props: Partial<MosaicaLayerInternalProps>) =>
      manager.setLayers([new MosaicaLayer({id: 'mosaica', depth: 2, status: 'shown', artifacts: artifacts(), ...props} as MosaicaLayerInternalProps)]),
    remove: () => manager.setLayers([]),
    buffer: () => {
      const data = marksLayer()?.props.data as {attributes: Record<string, {buffer?: FakeResource}>} | undefined;
      return data?.attributes['instancePositions']?.buffer ?? null;
    },
    texture: () => ((marksLayer()?.props as {lutTexture?: FakeResource | null} | undefined)?.lutTexture ?? null),
    /** The positions the marks sublayer was given as a typed array, where it was given no buffer. */
    cpuPositions: () => (marksLayer()?.props.data as {attributes: Record<string, {value?: Float32Array}>} | undefined)?.attributes['getPosition']?.value ?? null,
    /** The wash drawn: whether it shows, and the world bounds deck was given. */
    wash: () => {
      const props = sublayer('wash')?.props as {visible: boolean; bounds: number[]} | undefined;
      return props ? {visible: props.visible, bounds: props.bounds} : null;
    }
  };
}

describe('MosaicaLayer GPU resources', () => {
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

  it('draws through the slab the host passes and never releases it', () => {
    const h = host();
    const slab = new MarkSlab();
    slab.attach(h.device);
    h.draw({marks: marks(3), slab});
    h.draw({marks: marks(3), slab, scheme: 'light'});
    expect(h.errors).toEqual([]);
    const buffer = slab.layers()[0]!.draw.gpu!.positions as unknown as FakeResource;
    expect(h.buffer()).toBe(buffer);
    h.remove();
    expect(buffer.destroyed).toBe(false);
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

  it('leaves a slab the host never attached on the CPU path', () => {
    const h = host();
    const slab = new MarkSlab();
    h.draw({marks: marks(3), slab});
    expect(h.errors).toEqual([]);
    const draw = slab.layers()[0]!.draw;
    expect(draw.gpu).toBeNull();
    expect(h.buffer()).toBeNull();
    expect(h.cpuPositions()).toBe(draw.positions);
  });

});

describe('MosaicaLayer density wash', () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  const DEPTH = 3;
  const counts = (x: number, y: number): DensityCounts => {
    const span = WORLD_SIZE / 2 ** DEPTH;
    return {depth: DEPTH, cells: [{x, y, position: [(x + 0.5) * span, (y + 0.5) * span], count: 50}]};
  };
  /** The bounds deck is given for a wash built from these counts: `[left, bottom, right, top]`. */
  const drawnBounds = (c: DensityCounts) => {
    const [x0, y0, x1, y1] = filterDensity(binDensity(c)!, DEPTH).bounds;
    return [x0, y1, x1, y0];
  };

  it('is built and drawn per layer: two maps on one page neither cancel nor borrow each other’s', () => {
    vi.useFakeTimers();
    vi.stubGlobal('ImageData', class {
      constructor(readonly data: Uint8ClampedArray, readonly width: number, readonly height: number) {}
    });
    const a = host();
    const b = host();
    const a1 = counts(0, 0);
    const a2 = counts(2, 2);
    const onB = counts(6, 6);
    const drawA = (c: DensityCounts) => a.draw({marks: marks(3), depth: DEPTH, densityCounts: c});
    const drawB = () => b.draw({marks: marks(3), depth: DEPTH, densityCounts: onB});

    drawA(a1);
    drawB();
    vi.advanceTimersByTime(1000);
    drawA(a1);
    drawB();
    expect(a.wash()).toEqual({visible: true, bounds: drawnBounds(a1)});
    expect(b.wash()).toEqual({visible: true, bounds: drawnBounds(onB)});

    // While A's next wash is built, A draws its own last one.
    drawA(a2);
    expect(a.wash()).toEqual({visible: true, bounds: drawnBounds(a1)});
    vi.advanceTimersByTime(1000);
    drawA(a2);
    expect(a.wash()).toEqual({visible: true, bounds: drawnBounds(a2)});
    expect(a.errors).toEqual([]);
    expect(b.errors).toEqual([]);
  });
});
