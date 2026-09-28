import {describe, expect, it} from 'vitest';
import {LayerManager, type Layer} from '@deck.gl/core';
import {type ArtifactsProjection, type MarksProjection} from '@tesseradb/client';
import {SessionArtifactTable, servedLineage} from '@tesseradb/client/internal';
import {band} from '../../core/test/support.js';
import {TesseraLayer, type TesseraLayerInternalProps} from '../src/layer.js';
import {DULL_COLOUR} from '../src/marks-layer.js';
import {fakeDevice} from './fake-device.js';

/**
 * Under a highlight the marks are drawn in three passes: the rest in grey, then a glow under the
 * highlighted marks, then the highlighted marks in their own colour. What each pass was told is read
 * off the sublayers the Tessera layer rendered; the shader that acts on it is exercised in the
 * browser.
 */

const marks: MarksProjection = {bands: [band(2, 1n, 4, {highlightBits: Uint8Array.from([1, 0, 0, 1])})], standIn: [], count: {shown: 4, total: 4, exact: true}};

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

type PassProps = {visible: boolean; highlighting: boolean; highlightPass: string; dullColour: number[]; pickable: boolean};

function passes(props: Partial<TesseraLayerInternalProps>): Record<string, PassProps> {
  const manager = new LayerManager(fakeDevice(), {});
  manager.setLayers([new TesseraLayer({id: 'tessera', depth: 2, status: 'shown', artifacts: artifacts(), marks, density: 'none', ...props} as TesseraLayerInternalProps)]);
  const layer = manager.getLayers().find((l) => l.id === 'tessera') as TesseraLayer;
  const out: Record<string, PassProps> = {};
  for (const sub of layer.getSubLayers() as Layer[]) {
    const m = /^tessera-marks-p0(?:-(\w+))?$/.exec(sub.id);
    if (m) out[m[1] ?? 'all'] = sub.props as unknown as PassProps;
  }
  return out;
}

describe('highlight rendering', () => {
  it('draws the rest grey, then a glow, then the highlighted marks, and picks from the marks only', () => {
    const drawn = passes({highlighting: true, scheme: 'light'});
    expect(Object.keys(drawn)).toEqual(['dull', 'glow', 'lit']);
    for (const pass of Object.values(drawn)) {
      expect(pass.highlighting).toBe(true);
      expect(pass.dullColour).toEqual(DULL_COLOUR.light);
    }
    expect([drawn.dull!.pickable, drawn.glow!.pickable, drawn.lit!.pickable]).toEqual([true, false, true]);
  });

  it('greys the rest in the grey of the ground', () => {
    expect(passes({highlighting: true, scheme: 'dark'}).dull!.dullColour).toEqual(DULL_COLOUR.dark);
    expect(DULL_COLOUR.dark).not.toEqual(DULL_COLOUR.light);
  });

  it('draws one pass, highlighting nothing, when no highlight is set', () => {
    const drawn = passes({highlighting: false});
    expect(Object.keys(drawn)).toEqual(['all']);
    expect(drawn.all!.highlighting).toBe(false);
  });
});
