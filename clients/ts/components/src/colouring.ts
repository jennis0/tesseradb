import type {Store} from '@tesseradb/client';
import {DEFAULT_COLOURING, DEFAULT_SIZING, type Colouring, type Sizing} from '@tesseradb/deck';

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

/** Change the size choices for `store` and tell every element watching them. */
export function setSizing(store: Store, patch: Partial<Sizing>): void {
  const held = entry(store);
  const next = {...held.sizing, ...patch};
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
