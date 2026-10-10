import {describe, expect, it} from 'vitest';
import {Bool, Dictionary, Field, Float64, List, Table, Uint16, Uint32, Uint64, Uint8, Utf8, makeData, makeVector, tableToIPC, vectorFromArray} from 'apache-arrow';
import {checkArtifactsTrailer, decodeArtifactsFrame} from '../src/decode.js';
import {refused} from './support.js';

/**
 * One `/v1/artifacts/viewport` frame: eighteen columns at fixed positions, `layer`
 * dictionary-encoded and `tile` and `slot` last. Frames are built with apache-arrow's own writer, so these
 * check the decoder's reading of the layout; the captured goldens check the server's framing.
 */

const u64 = (values: bigint[]) => makeVector(makeData({type: new Uint64(), data: BigUint64Array.from(values)}));
const TEXTS = new List(new Field('item', new Utf8(), true));
const PARENTS = new List(new Field('item', new Uint64(), false));
const LAYER = new Dictionary(new Utf8(), new Uint16());

type Row = {layer?: string; id: bigint; rung?: number; matched?: boolean | null; highlighted?: boolean | null; parentIds?: bigint[]; target?: bigint | null; tile?: number | null; centroid?: [number, number] | null; slot?: number | null};

function columns(rows: Row[], layerType: unknown = LAYER): Record<string, unknown> {
  return {
    layer: vectorFromArray(rows.map((r) => r.layer ?? 'clusters/x'), layerType as never),
    mosaica_id: u64(rows.map((r) => r.id)),
    key: vectorFromArray(rows.map((r) => `c-${r.id}`), new Utf8()),
    masked_count: u64(rows.map(() => 7n)),
    centroid_x: vectorFromArray(rows.map((r) => (r.centroid === null ? null : (r.centroid?.[0] ?? 4))), new Float64()),
    centroid_y: vectorFromArray(rows.map((r) => (r.centroid === null ? null : (r.centroid?.[1] ?? 4))), new Float64()),
    box_min_x: vectorFromArray(rows.map(() => 0), new Uint32()),
    box_min_y: vectorFromArray(rows.map(() => 0), new Uint32()),
    box_max_x: vectorFromArray(rows.map(() => 9), new Uint32()),
    box_max_y: vectorFromArray(rows.map(() => 9), new Uint32()),
    content: vectorFromArray(rows.map(() => ['a name']), TEXTS),
    parent_ids: vectorFromArray(rows.map((r) => r.parentIds ?? []), PARENTS),
    rung: vectorFromArray(rows.map((r) => r.rung ?? 0), new Uint32()),
    matched: vectorFromArray(rows.map((r) => r.matched ?? null), new Bool()),
    highlighted: vectorFromArray(rows.map((r) => r.highlighted ?? null), new Bool()),
    target: vectorFromArray(rows.map((r) => r.target ?? null), new Uint64()),
    tile: vectorFromArray(rows.map((r) => (r.tile === undefined ? 5 : r.tile)), new Uint32()),
    slot: vectorFromArray(rows.map((r) => r.slot ?? null), new Uint8())
  };
}

const frame = (cols: Record<string, unknown>) => tableToIPC(new Table(cols as never), 'stream');

describe('an artifacts frame', () => {
  it('reads every column by its place, the layer resolved from its dictionary', () => {
    const decoded = decodeArtifactsFrame(frame(columns([{id: 1n, rung: 2, matched: true, highlighted: false, parentIds: [3n, 9n], target: 4n, slot: 6}])));
    expect(decoded.tile).toBe(5n);
    expect(decoded.artifacts).toEqual([
      {
        layer: 'clusters/x',
        mosaicaId: 1n,
        key: 'c-1',
        maskedCount: 7n,
        centroid: [4, 4],
        box: [0, 0, 9, 9],
        content: ['a name'],
        parentIds: [3n, 9n],
        rung: 2,
        matched: true,
        highlighted: false,
        target: 4n,
        slot: 6
      }
    ]);
  });

  it('reads a plain utf8 layer the same way: the encoding moves no information', () => {
    expect(decodeArtifactsFrame(frame(columns([{id: 1n}], new Utf8()))).artifacts[0]!.layer).toBe('clusters/x');
  });

  it('reads null bits as no question asked, a null centroid as none declared, a null target as attached to nothing and a null slot as no palette asked for', () => {
    const [a] = decodeArtifactsFrame(frame(columns([{id: 1n, centroid: null}]))).artifacts;
    expect([a!.matched, a!.highlighted, a!.target, a!.centroid, a!.slot]).toEqual([null, null, null, null, null]);
  });

  it('names no tile for a frame of no rows, and none for the treed frame', () => {
    expect(decodeArtifactsFrame(frame(columns([])))).toEqual({tile: null, artifacts: []});
    expect(decodeArtifactsFrame(frame(columns([{id: 1n, tile: null}, {id: 2n, tile: null}]))).tile).toBeNull();
  });

  it('refuses rows naming two tiles: a frame answers one', () => {
    refused(() => decodeArtifactsFrame(frame(columns([{id: 1n, tile: 5}, {id: 2n, tile: 6}]))));
  });

  it('refuses a column missing, added or out of place: a server and a client of different versions', () => {
    const without = columns([{id: 1n}]);
    delete without['tile'];
    refused(() => decodeArtifactsFrame(frame(without)));
    const shaped = {...columns([{id: 1n}]), shape_x: vectorFromArray([null], new Uint32())};
    refused(() => decodeArtifactsFrame(frame(shaped)));
    const {layer, ...rest} = columns([{id: 1n}]);
    refused(() => decodeArtifactsFrame(frame({...rest, layer})));
  });
});

describe('the artifacts trailer', () => {
  const trailer = (fields: Record<string, unknown>) => new TextEncoder().encode(JSON.stringify(fields));

  it('agrees with the frames and rows the body carried', () => {
    expect(() => checkArtifactsTrailer(trailer({stream_us: 1, arrow_serialise_ns: 2, rows: 3, frames: 2}), 2, 3)).not.toThrow();
    refused(() => checkArtifactsTrailer(trailer({stream_us: 1, arrow_serialise_ns: 2, rows: 3, frames: 2}), 2, 4));
    refused(() => checkArtifactsTrailer(trailer({stream_us: 1, arrow_serialise_ns: 2, rows: 3, frames: 2}), 1, 3));
  });

  it('refuses a key outside its closed set', () => {
    refused(() => checkArtifactsTrailer(trailer({stream_us: 1, arrow_serialise_ns: 2, rows: 0, frames: 0, points: 0}), 0, 0));
  });
});
