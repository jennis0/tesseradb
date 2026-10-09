import type {Store} from '@mosaicajs/client';
import {chosenColour} from '@mosaicajs/client/internal';
import {DEFAULT_COLOURING, DEFAULT_SIZING, type Colouring, type Sizing} from '@mosaicajs/deck';
import {UNMAPPED, colourOfRank, hexOf} from '@mosaicajs/deck/internal';

/**
 * The colour and size choices of every element reading one store: the palette, the ramp, the
 * colours chosen for single values, and the sizes and scale a number column sizes points on. The
 * map draws with them and the legend shows and changes them, and the two may be siblings that share
 * nothing but the store, so the choices are kept per store here. They are presentation only:
 * nothing here changes what is fetched, counted or drawn.
 *
 * A value colour names a category key the server answered for one viewer. When the store forgets
 * what the server answered (its `meta` goes to `null`, on `clear()` or an answer under another
 * identity), the value colours go too, so the next viewer's values take none of them. The palette,
 * the ramp, its scale and its direction are the user's and stay.
 */

type Entry = {colouring: Colouring; sizing: Sizing; listeners: Set<() => void>; metaHeld: boolean};

const entries = new WeakMap<Store, Entry>();

function entry(store: Store): Entry {
  let held = entries.get(store);
  if (!held) {
    const entry: Entry = {colouring: DEFAULT_COLOURING, sizing: DEFAULT_SIZING, listeners: new Set(), metaHeld: store.get('meta') !== null};
    store.subscribe(() => {
      const meta = store.get('meta') !== null;
      const forgot = entry.metaHeld && !meta;
      entry.metaHeld = meta;
      if (forgot && Object.keys(entry.colouring.values).length > 0) setColouring(store, {values: {}});
    });
    entries.set(store, entry);
    held = entry;
  }
  return held;
}

/** The colour choices for `store`, the defaults where none were made or there is no store. */
export function colouringOf(store: Store | null): Colouring {
  return store ? entry(store).colouring : DEFAULT_COLOURING;
}

/** Change the colour choices for `store` and tell every element watching them. */
export function setColouring(store: Store, patch: Partial<Colouring>): void {
  const held = entry(store);
  const next = {...held.colouring, ...patch};
  const same = (Object.keys(patch) as (keyof Colouring)[]).every((k) => held.colouring[k] === next[k]);
  if (same) return;
  held.colouring = next;
  for (const fn of [...held.listeners]) fn();
}

/** The size choices for `store`, the defaults where none were made or there is no store. */
export function sizingOf(store: Store | null): Sizing {
  return store ? entry(store).sizing : DEFAULT_SIZING;
}

/**
 * Change the size choices for `store` and tell every element watching them. A radius that is not a
 * finite number above zero is ignored, as a budget that is not is, and so is a scale it does not
 * name.
 */
export function setSizing(store: Store, patch: Partial<Sizing>): void {
  const held = entry(store);
  const radius = (r: number | undefined): r is number => typeof r === 'number' && Number.isFinite(r) && r > 0;
  const next = {...held.sizing, ...(radius(patch.min) ? {min: patch.min} : {}), ...(radius(patch.max) ? {max: patch.max} : {}), ...(patch.scale === 'linear' || patch.scale === 'log' || patch.scale === 'rank' ? {scale: patch.scale} : {})};
  if (next.min === held.sizing.min && next.max === held.sizing.max && next.scale === held.sizing.scale) return;
  held.sizing = next;
  for (const fn of [...held.listeners]) fn();
}

/** Call `fn` whenever the colour or size choices for `store` change. Returns the function that stops it. */
export function watchChoices(store: Store, fn: () => void): () => void {
  const held = entry(store);
  held.listeners.add(fn);
  return () => held.listeners.delete(fn);
}

/** `values` with `key` of `column` given `colour` (`#rrggbb`), or its chosen colour removed where `colour` is null. */
export function withValueColour(values: Colouring['values'], column: string, key: string, colour: string | null): Colouring['values'] {
  const own = {...(values[column] ?? {})};
  if (colour === null) delete own[key];
  else own[key] = colour;
  const next = {...values, [column]: own};
  if (Object.keys(own).length === 0) delete next[column];
  return next;
}

/** A colour given to one category value, `#rrggbb`, or `null` for its palette colour. */
export type ValueChange = {value: string; colour: string | null};

/** Give each value of `column` that `changes` names its colour, or its palette colour back. */
export function setValueColours(store: Store, column: string, changes: readonly ValueChange[]): void {
  let values = colouringOf(store).values;
  for (const c of changes) values = withValueColour(values, column, c.value, c.colour);
  setColouring(store, {values});
}

/** Whether the map has met the value `key` of `column`, and so given it a palette colour by its rank. */
export function valueMet(store: Store, column: string, key: string): boolean {
  return (store.get('legend').categories[column] ?? []).some((v) => v.key === key);
}

/** A category value's palette colour as the map draws it, by its rank, `#rrggbb`; grey for a value the map has not met. */
export function paletteValueColour(store: Store, column: string, key: string): string {
  const legend = store.get('legend');
  const code = (legend.categories[column] ?? []).find((v) => v.key === key)?.code;
  return hexOf(code === undefined ? UNMAPPED : colourOfRank(legend.ranks[column]?.[code], colouringOf(store).palette));
}

/** A category value's colour on the map, `#rrggbb`: the one chosen for it, else its palette colour. */
export function valueColour(store: Store, column: string, key: string): string {
  return colouringOf(store).values[column]?.[key] ?? paletteValueColour(store, column, key);
}

/** A colour given to one cluster by `mosaica_id`, `#rrggbb`, or `null` for its palette colour. */
export type ClusterChange = {mosaicaId: bigint; colour: string | null};

/** Give each cluster of `layer` that `changes` names its colour, or its palette colour back, keeping every other chosen colour. */
export function setClusterColours(store: Store, layer: string, changes: readonly ClusterChange[]): void {
  const held = store.get('artifacts').overrides;
  const own = new Map(held.get(layer));
  for (const c of changes) {
    const rgba = c.colour === null ? null : chosenColour(c.colour);
    if (rgba === null) own.delete(c.mosaicaId);
    else own.set(c.mosaicaId, rgba);
  }
  store.setArtifactColours(new Map([...held, [layer, own]]));
}

/** A cluster's colour on the map, `#rrggbb`: the one chosen for it in `layer`, else `own`, its palette colour. */
export function clusterColour(store: Store, layer: string, id: bigint, own: string): string {
  const chosen = store.get('artifacts').overrides.get(layer)?.get(id);
  return chosen ? hexOf(chosen) : own;
}

/**
 * Note the value and cluster colours chosen over `store` now, returning what puts them back, for
 * a colour shown while it is dragged and then given up.
 */
export function holdColours(store: Store): () => void {
  const values = colouringOf(store).values;
  const clusters = store.get('artifacts').overrides;
  return () => {
    setColouring(store, {values});
    if (store.get('artifacts').overrides !== clusters) store.setArtifactColours(clusters);
  };
}
