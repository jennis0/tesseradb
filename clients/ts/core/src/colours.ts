import type {SessionArtifactTable} from './artifactTable.js';
import {artifactColours, positionalEntry, type PaletteKind, type PaletteScheme, type Rgba} from './palette.js';

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
