import type {Store} from '@tesseradb/client';
import {DEFAULT_COLOURING, type Colouring} from '@tesseradb/deck';

/**
 * The colour choices of every element reading one store: the palette, the ramp and the colours
 * chosen for single values. The map draws with them and the legend shows and changes them, and the
 * two may be siblings that share nothing but the store, so the choices are kept per store here.
 * They are presentation only: nothing here changes what is fetched, counted or drawn.
 */

type Entry = {colouring: Colouring; listeners: Set<() => void>};

const entries = new WeakMap<Store, Entry>();

function entry(store: Store): Entry {
  let held = entries.get(store);
  if (!held) {
    held = {colouring: DEFAULT_COLOURING, listeners: new Set()};
    entries.set(store, held);
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

/** Call `fn` whenever the colour choices for `store` change. Returns the function that stops it. */
export function watchColouring(store: Store, fn: () => void): () => void {
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
