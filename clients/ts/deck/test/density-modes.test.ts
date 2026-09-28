import {afterEach, beforeAll, beforeEach, describe, expect, it, vi} from 'vitest';
import {LayerManager, type Layer} from '@deck.gl/core';
import {WORLD_SIZE, type ArtifactsProjection, type ComposedTile, type MarksProjection, type TilesProjection} from '@tesseradb/client';
import {SessionArtifactTable, mortonOfTile, servedLineage} from '@tesseradb/client/internal';
import {band} from '../../core/test/support.js';
import {contourThresholds, densityStops, type DensityMode} from '../src/density.js';
import {TesseraLayer, loadAggregationLayers, type TesseraLayerInternalProps} from '../src/layer.js';
import {fakeDevice} from './fake-device.js';

/**
 * Every density mode draws the server's exact tile counts. The marks are a sample capped per tile,
 * so the tests serve a sample that disagrees with the counts: a tile with three marks and a large
 * count beside a tile with many marks and a small one. What each mode was given to draw is read off
 * the sublayer the Tessera layer rendered.
 */

const DEPTH = 3;
const SPAN = WORLD_SIZE / 2 ** DEPTH;

const tile = (x: number, y: number, exact: boolean, visible: number, matched = visible, highlighted = matched, depth = DEPTH): ComposedTile => ({
  prefix: mortonOfTile(x, y, depth),
  depth,
  exact,
  drawn: 10,
  counts: exact ? {visible: BigInt(visible), matched: BigInt(matched), highlighted: BigInt(highlighted), served: 10} : null
});

/** Many marks where the count is small, few where it is large. */
const tiles: TilesProjection = {
  tiles: [tile(1, 1, true, 90_000, 80_000, 10), tile(2, 1, true, 400, 300, 200), tile(5, 5, false, 7), tile(0, 0, true, 5, 5, 5, DEPTH + 1)]
};
const marks: MarksProjection = {
  bands: [band(DEPTH, mortonOfTile(1, 1, DEPTH), 3), band(DEPTH, mortonOfTile(2, 1, DEPTH), 400)],
  standIn: [],
  count: {shown: 403, total: 80_300, exact: true}
};

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

type Cell = {position: [number, number]; count: number};

function host() {
  const manager = new LayerManager(fakeDevice(), {});
  const errors: unknown[] = [];
  manager.setProps({onError: (error: unknown, layer: Layer) => (layer.id === 'tessera' ? errors.push(error) : undefined)});
  const sublayer = (id: string) => {
    const layer = manager.getLayers().find((l) => l.id === 'tessera') as TesseraLayer | undefined;
    return (layer?.getSubLayers() as Layer[] | undefined)?.find((l) => l.id === `tessera-${id}`);
  };
  const props = <T>(id: string) => sublayer(id)?.props as unknown as T;
  return {
    errors,
    sublayer,
    props,
    /** Draw, and draw again once the cells have settled, as a host's next frame would. */
    draw: (props: Partial<TesseraLayerInternalProps>) => {
      const layer = () => new TesseraLayer({id: 'tessera', depth: DEPTH, status: 'shown', artifacts: artifacts(), marks, tiles, ...props} as TesseraLayerInternalProps);
      manager.setLayers([layer()]);
      vi.advanceTimersByTime(1000);
      manager.setLayers([layer()]);
    },
    /** Draw once, with no time to settle. */
    drawNow: (props: Partial<TesseraLayerInternalProps>) =>
      manager.setLayers([new TesseraLayer({id: 'tessera', depth: DEPTH, status: 'shown', artifacts: artifacts(), marks, tiles, ...props} as TesseraLayerInternalProps)]),
    /** The grid's squares: each one's centre and the index of its colour among `stops`' steps. */
    squares: () => {
      const props = sublayer('density-grid')?.props as unknown as {data: {polygon: [number, number][]; colour: number[]}[]} | undefined;
      return props?.data.map((d) => ({centre: [(d.polygon[0]![0] + d.polygon[2]![0]) / 2, (d.polygon[0]![1] + d.polygon[2]![1]) / 2], width: d.polygon[1]![0] - d.polygon[0]![0], colour: d.colour})) ?? null;
    },
    /** What an aggregation sublayer was given: each cell's position and the weight its accessor reads. */
    aggregated: (id: string): {position: [number, number]; weight: number}[] | null => {
      const props = sublayer(id)?.props as {data: Cell[]; getPosition: (d: Cell) => [number, number]; getColorWeight?: (d: Cell) => number; getWeight?: (d: Cell) => number} | undefined;
      if (!props) return null;
      const weight = props.getColorWeight ?? props.getWeight!;
      return props.data.map((d) => ({position: props.getPosition(d), weight: weight(d)}));
    }
  };
}

const centre = (x: number, y: number): [number, number] => [(x + 0.5) * SPAN, (y + 0.5) * SPAN];

describe('density modes', () => {
  beforeAll(async () => {
    await loadAggregationLayers();
  });
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal('ImageData', class {
      constructor(readonly data: Uint8ClampedArray, readonly width: number, readonly height: number) {}
    });
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it.each<[DensityMode, string]>([
    ['hex', 'density-hex'],
    ['contours', 'density-contours']
  ])('%s aggregates one point per exact tile at the drawn depth, weighted by its count, whatever the sample holds', (density, id) => {
    const h = host();
    h.draw({density, densityChannel: 'matched'});
    expect(h.errors).toEqual([]);
    // The non-exact tile and the tile at another depth give nothing; the weights are the counts,
    // with the three-mark tile the heavier by far.
    expect(h.aggregated(id)).toEqual([
      {position: centre(1, 1), weight: 80_000},
      {position: centre(2, 1), weight: 300}
    ]);
  });

  it('reads the count the channel names, so a highlight draws where its members are', () => {
    const h = host();
    h.draw({density: 'hex', densityChannel: 'highlighted'});
    expect(h.aggregated('density-hex')!.map((c) => c.weight)).toEqual([10, 200]);
    h.draw({density: 'hex', densityChannel: 'visible'});
    expect(h.aggregated('density-hex')!.map((c) => c.weight)).toEqual([90_000, 400]);
  });

  it('draws the grid as one square per exact tile, coloured by the tile’s count, whatever the sample holds', () => {
    const h = host();
    h.draw({density: 'grid', densityChannel: 'matched', densityColours: 'greys', scheme: 'light'});
    const squares = h.squares()!;
    expect(squares.map((q) => q.centre)).toEqual([centre(1, 1), centre(2, 1)]);
    for (const q of squares) expect(q.width).toBeCloseTo(SPAN * 0.94);
    // On a light ground the dense end of Greys is the darker: the three-mark tile of 80,000.
    const lum = (c: number[]) => c[0]! + c[1]! + c[2]!;
    expect(lum(squares[0]!.colour)).toBeLessThan(lum(squares[1]!.colour));
  });

  it('bins hexagons one tile wide, so no bin is finer than the counts', () => {
    const h = host();
    h.draw({density: 'hex'});
    expect(h.props<{radius: number}>('density-hex').radius).toBe(SPAN);
  });

  it('keeps the bins it drew while tiles stream, and redraws once they settle', () => {
    const h = host();
    h.draw({density: 'grid'});
    expect(h.squares()).toHaveLength(2);
    const next: TilesProjection = {tiles: [tile(4, 4, true, 10)]};
    h.drawNow({density: 'grid', tiles: next});
    expect(h.squares()).toHaveLength(2);
    h.draw({density: 'grid', tiles: next});
    expect(h.squares()!.map((q) => q.centre)).toEqual([centre(4, 4)]);
  });

  it('draws contours at counts taken from the tiles', () => {
    const h = host();
    h.draw({density: 'contours'});
    const contours = h.props<{contours: {threshold: number}[]}>('density-contours').contours;
    expect(contours.map((c) => c.threshold)).toEqual(contourThresholds([
      {position: centre(1, 1), count: 80_000},
      {position: centre(2, 1), count: 300}
    ]));
  });

  it('draws no aggregation layer under none or smooth, and the wash only under smooth', () => {
    const h = host();
    for (const density of ['none', 'smooth'] as const) {
      h.draw({density});
      expect(h.sublayer('density-hex') ?? h.sublayer('density-grid') ?? h.sublayer('density-contours')).toBeUndefined();
      expect((h.sublayer('wash')!.props as {visible: boolean}).visible).toBe(density === 'smooth');
    }
  });

  it('draws density and no marks with the points off', () => {
    const h = host();
    h.draw({density: 'grid', points: false});
    expect(h.errors).toEqual([]);
    expect(h.squares()).toHaveLength(2);
    const drawnMarks = (h.sublayer('marks-p0')?.props as {visible: boolean} | undefined)?.visible ?? false;
    expect(drawnMarks).toBe(false);
    h.draw({density: 'grid', points: true});
    expect((h.sublayer('marks-p0')!.props as {visible: boolean}).visible).toBe(true);
  });

  it('sets the names on plates over hexagons, a grid or a ramped wash, and keeps the halo elsewhere', () => {
    const h = host();
    for (const [density, plated] of [['hex', true], ['grid', true], ['smooth', false], ['contours', false], ['none', false]] as const) {
      h.draw({density});
      expect(h.sublayer('labels-plates') !== undefined).toBe(plated);
    }
    h.draw({density: 'smooth', densityColours: 'viridis'});
    expect(h.sublayer('labels-plates')).toBeDefined();
  });

  it('draws at the strength asked for', () => {
    const h = host();
    h.draw({density: 'hex', densityStrength: 0.4});
    expect(h.sublayer('density-hex')!.props.opacity).toBe(0.4);
  });
});

describe('densityStops', () => {
  const lum = ([r, g, b]: readonly number[]) => 0.2126 * r! + 0.7152 * g! + 0.0722 * b!;

  it('puts the dense end away from the ground: dark on a light map, light on a dark one', () => {
    for (const colours of ['warm-grey', 'viridis', 'cividis', 'magma', 'greys'] as const) {
      const light = densityStops(colours, 'light');
      const dark = densityStops(colours, 'dark');
      expect(lum(light.at(-1)!)).toBeLessThan(lum(light[0]!));
      expect(lum(dark.at(-1)!)).toBeGreaterThan(lum(dark[0]!));
    }
  });
});
