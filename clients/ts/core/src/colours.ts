import type {SessionArtifactTable} from './artifactTable.js';
import {artifactColours, slottedColour, type ChosenColours, type PaletteName, type Rgba} from './palette.js';

/**
 * A colour per live ordinal of the session artifact table, rebuilt only when the table has moved.
 *
 * An ordinal's colour depends on its own slot, the palette size the slot was served under and the
 * colour chosen for it alone, so the map is extended in place for the table's changes and keeps its
 * identity; a lookup texture reads identity to tell an extension from a recolour. A change of the
 * chosen colours rebuilds the map. A change of palette recolours nothing by itself: each artifact
 * keeps the colour of its slot until a slot of the new size is served for it.
 */
export class ArtifactColours {
  private name: PaletteName;
  private chosen: ChosenColours = new Map();
  /** The table version the map was built at. */
  private builtAt = -1;
  /** Whether {@link map} was built under the current chosen colours. */
  private valid = false;
  private map = new Map<number, Rgba>();

  constructor(
    private readonly table: SessionArtifactTable,
    palette: PaletteName,
    private readonly publish: (colours: ReadonlyMap<number, Rgba>, palette: PaletteName, chosen: ChosenColours) => void
  ) {
    this.name = palette;
  }

  get palette(): PaletteName {
    return this.name;
  }

  /** The colours set by layer and `mosaica_id` in place of the palette's. */
  get overrides(): ChosenColours {
    return this.chosen;
  }

  /** The colours, brought up to date with the table. */
  current(): ReadonlyMap<number, Rgba> {
    return this.table.version === this.builtAt && this.valid ? this.map : this.build();
  }

  /** Publish the colours if the table has named, placed or freed an artifact since they were built. */
  refresh(): void {
    if (this.table.version === this.builtAt) return;
    this.publish(this.build(), this.name, this.chosen);
  }

  setPalette(name: PaletteName): void {
    if (name === this.name) return;
    this.name = name;
    this.publish(this.current(), this.name, this.chosen);
  }

  /** Colour the artifacts `chosen` names with its colours in place of the palette's, and no others. */
  setOverrides(chosen: ChosenColours): void {
    // A layer with no colour chosen is left out, so an empty layer reads as none.
    this.chosen = new Map([...chosen].filter(([, ids]) => ids.size > 0).map(([layer, ids]) => [layer, new Map(ids)]));
    this.valid = false;
    this.publish(this.build(), this.name, this.chosen);
  }

  private build(): Map<number, Rgba> {
    const {table, chosen} = this;
    const changes = this.valid ? table.changesSince(this.builtAt) : null;
    this.builtAt = table.version;
    this.valid = true;
    if (changes) {
      for (const {ordinal, kind} of changes) {
        const entry = table.entry(ordinal);
        if (kind === 'freed' || !entry) this.map.delete(ordinal);
        else this.map.set(ordinal, slottedColour(entry, chosen));
      }
      return this.map;
    }
    this.map = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, layer: entry.layer, mosaicaId: entry.mosaicaId, slot: entry.slot, paletteSize: entry.paletteSize})),
      chosen
    );
    return this.map;
  }
}
