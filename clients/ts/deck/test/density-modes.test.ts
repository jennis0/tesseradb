import {afterEach, beforeAll, beforeEach, describe, expect, it, vi} from 'vitest';
import {LayerManager, type Layer} from '@deck.gl/core';
import {WORLD_SIZE, type ArtifactsProjection, type MarksProjection} from '@tesseradb/client';
import {SessionArtifactTable, mortonOfTile, servedLineage} from '@tesseradb/client/internal';
import {band} from '../../core/test/support.js';
import {contourThresholds, densityStops, type DensityCell, type DensityCounts, type DensityMode} from '../src/density.js';
import {TesseraLayer, loadAggregationLayers, type TesseraLayerInternalProps} from '../src/layer.js';
import {fakeDevice} from './fake-device.js';

/**
 * Every density mode draws the counts by cell it is given. The marks are a sample capped per tile,
 * so the tests serve a sample that disagrees with the counts: a cell with three marks and a large
 * count beside a cell with many marks and a small one. What each mode was given to draw is read off
 * the sublayer the Tessera layer rendered.
 */

const DEPTH = 3;
/** The counts' depth, finer than the marks' bands. */
const CELLS = DEPTH + 2;
const SPAN = WORLD_SIZE / 2 ** CELLS;

const cell = (x: number, y: number, count: number): DensityCell => ({x, y, position: [(x + 0.5) * SPAN, (y + 0.5) * SPAN], count});

/** Few marks where the count is large, many where it is small. */
const counts: DensityCounts = {depth: CELLS, cells: [cell(4, 4, 80_000), cell(8, 4, 300)]};
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
    palette: 'positional',
    coverage: {current: 0, stale: 0}
  };
}

type Cell = DensityCell;

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
      const layer = () => new TesseraLayer({id: 'tessera', depth: DEPTH, status: 'shown', artifacts: artifacts(), marks, densityCounts: counts, ...props} as TesseraLayerInternalProps);
      manager.setLayers([layer()]);
      vi.advanceTimersByTime(1000);
      manager.setLayers([layer()]);
    },
    /** Draw once, with no time to settle. */
    drawNow: (props: Partial<TesseraLayerInternalProps>) =>
      manager.setLayers([new TesseraLayer({id: 'tessera', depth: DEPTH, status: 'shown', artifacts: artifacts(), marks, densityCounts: counts, ...props} as TesseraLayerInternalProps)]),
    /**
     * The grid's colour at a world point, `[r, g, b, a]`, read from the image it draws; `null`
     * where no grid is drawn.
     */
    gridAt: (world: [number, number]): number[] | null => {
      const props = sublayer('density-grid')?.props as unknown as {visible: boolean; image: {data: Uint8ClampedArray; width: number; height: number}; bounds: [number, number, number, number]} | undefined;
      if (!props?.visible) return null;
      const [left, bottom, right, top] = props.bounds;
      const {data, width, height} = props.image;
      const px = Math.floor(((world[0] - left) / (right - left)) * width);
      const py = Math.floor(((world[1] - top) / (bottom - top)) * height);
      if (px < 0 || py < 0 || px >= width || py >= height) return [0, 0, 0, 0];
      const i = (py * width + px) * 4;
      return [...data.slice(i, i + 4)];
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
  ])('%s aggregates one point per cell, weighted by its count, whatever the sample holds', (density, id) => {
    const h = host();
    h.draw({density});
    expect(h.errors).toEqual([]);
    expect(h.aggregated(id)).toEqual([
      {position: centre(4, 4), weight: 80_000},
      {position: centre(8, 4), weight: 300}
    ]);
  });

  it('draws the grid as one image, a square per cell coloured by the cell’s count, whatever the sample holds', () => {
    const h = host();
    h.draw({density: 'grid', densityColours: 'greys', scheme: 'light'});
    const dense = h.gridAt(centre(4, 4))!;
    const sparse = h.gridAt(centre(8, 4))!;
    expect(dense[3]).toBe(255);
    expect(sparse[3]).toBe(255);
    // A cell with no count is clear.
    expect(h.gridAt(centre(6, 4))![3]).toBe(0);
    // On a light ground the dense end of Greys is the darker: the three-mark cell of 80,000.
    const lum = (c: number[]) => c[0]! + c[1]! + c[2]!;
    expect(lum(dense)).toBeLessThan(lum(sparse));
  });

  it('feeds the hexagons and contours at most their budget of cells, merged to a coarser depth, every item kept', () => {
    const h = host();
    const many: DensityCell[] = [];
    for (let x = 0; x < 160; x++) for (let y = 0; y < 100; y++) many.push(cell(x, y, 1));
    const fine = {depth: 10, cells: many.map((c) => ({...c, position: [(c.x + 0.5) * (WORLD_SIZE / 1024), (c.y + 0.5) * (WORLD_SIZE / 1024)] as [number, number]}))};
    for (const [density, id] of [['hex', 'density-hex'], ['contours', 'density-contours']] as const) {
      h.draw({density, densityCounts: fine});
      const fed = h.aggregated(id)!;
      expect(fed.length).toBeLessThan(many.length);
      expect(fed.length).toBeLessThanOrEqual(density === 'hex' ? 10_000 : 2_000);
      expect(fed.reduce((n, c) => n + c.weight, 0)).toBe(many.length);
    }
  });

  it('bins hexagons one cell wide at the counts’ depth, whatever the marks’ depth', () => {
    const h = host();
    h.draw({density: 'hex'});
    expect(h.props<{radius: number}>('density-hex').radius).toBe(SPAN);
  });

  it('draws the counts it is given and nothing without them', () => {
    const h = host();
    h.draw({density: 'grid'});
    expect(h.gridAt(centre(4, 4))![3]).toBe(255);
    h.draw({density: 'grid', densityCounts: {depth: CELLS, cells: [cell(1, 1, 10)]}});
    expect(h.gridAt(centre(1, 1))![3]).toBe(255);
    expect(h.gridAt(centre(4, 4))![3]).toBe(0);
    h.drawNow({density: 'grid', densityCounts: null});
    expect(h.gridAt(centre(1, 1))).toBeNull();
  });

  it('draws contours at counts taken from the cells', () => {
    const h = host();
    h.draw({density: 'contours'});
    const contours = h.props<{contours: {threshold: number}[]}>('density-contours').contours;
    expect(contours.map((c) => c.threshold)).toEqual(contourThresholds(counts.cells));
  });

  it('draws no aggregation layer under none or smooth, and the wash only under smooth', () => {
    const h = host();
    for (const density of ['none', 'smooth'] as const) {
      h.draw({density});
      expect(h.sublayer('density-hex') ?? h.sublayer('density-contours')).toBeUndefined();
      expect(h.gridAt(centre(4, 4))).toBeNull();
      expect((h.sublayer('wash')!.props as {visible: boolean}).visible).toBe(density === 'smooth');
    }
  });

  it('draws density and no marks with the points off', () => {
    const h = host();
    h.draw({density: 'grid', points: false});
    expect(h.errors).toEqual([]);
    expect(h.gridAt(centre(4, 4))![3]).toBe(255);
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
