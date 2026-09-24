import type {SessionArtifactTable} from './artifactTable.js';
import {bandKey, type Band, type BandKey} from './bands.js';
import {artifactColours, positionalEntry, type PaletteKind, type PaletteScheme, type Rgba} from './palette.js';
import {rectContainsTile, type TileRect} from './rects.js';

/**
 * A colour per live ordinal of the session artifact table, rebuilt only when the table has moved.
 *
 * Under `positional` an ordinal's colour depends on its own centroid alone, so the map is extended
 * in place for the table's changes and keeps its identity; a lookup texture reads identity to tell
 * an extension from a recolour. `spread` assigns hues by rank over the whole set, so any change
 * rebuilds the map, as does a change of palette or scheme.
 */
export class ArtifactColours {
  private kind: PaletteKind;
  private scheme: PaletteScheme = 'dark';
  /** The table version the map was built at. */
  private builtAt = -1;
  private builtUnder: {palette: PaletteKind; scheme: PaletteScheme} | null = null;
  private map = new Map<number, Rgba>();

  constructor(
    private readonly table: SessionArtifactTable,
    palette: PaletteKind,
    private readonly publish: (colours: ReadonlyMap<number, Rgba>, palette: PaletteKind) => void
  ) {
    this.kind = palette;
  }

  get palette(): PaletteKind {
    return this.kind;
  }

  /** The colours, brought up to date with the table. */
  current(): ReadonlyMap<number, Rgba> {
    return this.table.version === this.builtAt ? this.map : this.build();
  }

  /** Publish the colours if the table has named or freed an artifact since they were built. */
  refresh(): void {
    if (this.table.version === this.builtAt) return;
    this.publish(this.build(), this.kind);
  }

  setPalette(kind: PaletteKind): void {
    if (kind === this.kind) return;
    this.kind = kind;
    this.publish(this.build(), this.kind);
  }

  setScheme(scheme: PaletteScheme): void {
    if (scheme === this.scheme) return;
    this.scheme = scheme;
    this.publish(this.build(), this.kind);
  }

  private build(): Map<number, Rgba> {
    const {table, kind, scheme} = this;
    const extend = kind === 'positional' && this.builtUnder?.palette === 'positional' && this.builtUnder.scheme === scheme;
    const changes = extend ? table.changesSince(this.builtAt) : null;
    this.builtAt = table.version;
    this.builtUnder = {palette: kind, scheme};
    if (changes) {
      for (const {ordinal, kind: change} of changes) {
        if (change === 'freed') this.map.delete(ordinal);
        else this.map.set(ordinal, positionalEntry(table.entry(ordinal)?.centroid ?? null, scheme));
      }
      return this.map;
    }
    this.map = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      kind,
      scheme
    );
    return this.map;
  }
}

/**
 * Which bands in view draw every point in a colour. A band is colour-stale when it has no
 * membership column for a layer asked for, or names an ordinal that resolves to no colour; its
 * tile is then fetched again, once per served-set version.
 *
 * A band resolves against the colours, which cover every artifact the table holds, rather than
 * against the latest served set: a band fetched under a coarser cut names artifacts outside the
 * finer one and is still coloured correctly.
 */
export class ColourCoverage {
  /** The served-set version each band was last asked for again under. */
  private readonly asked = new Map<BandKey, number>();

  /** Forget what was asked for, when the bands drawn are another view's. */
  forget(): void {
    this.asked.clear();
  }

  /**
   * Count the bands in `visible` (every band where it is null) at `depth` whose ordinals all
   * resolve, and return the stale ones not yet asked for under `version`. With `mayAsk` false
   * nothing is returned or recorded, so the bands are decided at the next check.
   */
  check(input: {
    bands: readonly Band[];
    layers: readonly string[];
    table: SessionArtifactTable;
    colours: ReadonlyMap<number, Rgba>;
    version: number;
    visible: TileRect | null;
    depth: number;
    mayAsk: boolean;
  }): {current: number; stale: number; toAsk: Band[]} {
    const {bands, layers, table, colours, visible, depth, version} = input;
    const stale: Band[] = [];
    let current = 0;
    for (const band of bands) {
      if (visible && (band.depth !== depth || !rectContainsTile(visible, band.x, band.y))) continue;
      if (layers.every((layer) => resolves(band, layer, table, colours))) current++;
      else stale.push(band);
    }
    const toAsk = input.mayAsk ? stale.filter((b) => this.asked.get(bandKey(b.depth, b.prefix)) !== version) : [];
    for (const b of toAsk) this.asked.set(bandKey(b.depth, b.prefix), version);
    return {current, stale: stale.length, toAsk};
  }
}

function resolves(band: Band, layer: string, table: SessionArtifactTable, colours: ReadonlyMap<number, Rgba>): boolean {
  const m = band.membership[layer];
  if (!m) return false;
  for (let i = 0; i < m.distinct.length; i++) {
    if (table.resolve(m.distinct[i]!, colours) === 0) return false;
  }
  return true;
}
