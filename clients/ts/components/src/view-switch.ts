import type {Meta, Quantisation, Store} from '@tesseradb/client';
import {emit} from './base.js';

/**
 * What the two pickers share beyond the rules themselves (`view-switching.md` §6.3).
 *
 * **The rules live in `@tesseradb/client`** — `viewLabel`, `viewPickerEntries`, `enterGroup`,
 * `hasOneLayout`, `viewsOfGroup` and `stepView` — so a host that draws its own picker gets the
 * same answers as these elements without importing an element. What is left here is the frame
 * comparison the camera decides by and the one site a switch is announced from.
 */

/**
 * Whether two views are quantised against one frame — the field that decides whether a switch
 * keeps the camera or refits it (§4).
 *
 * Compared **by value**, never by reference: `/v1/meta` publishes each view's extent separately,
 * so the views of one group carry equal-but-distinct objects and a reference test would report
 * every step of a roster as a frame change.
 */
export function sameFrame(a: Quantisation | null, b: Quantisation | null): boolean {
  if (!a || !b) return a === b;
  return a.xMin === b.xMin && a.xMax === b.xMax && a.yMin === b.yMin && a.yMax === b.yMax;
}

/**
 * Make `id` the current view and announce it (`view-switching.md` §6.3).
 *
 * One site, so both pickers report the same event: `tessera-viewswitch` `{from, to, sameFrame}`,
 * bubbling and composed, for a host that owns the URL or a basemap. The map does not listen for
 * it — it reacts to the `view` projection, so a host calling `setCurrentView` itself gets the
 * same refit.
 */
export function switchView(from: HTMLElement, store: Store, meta: Meta, id: string): void {
  const current = store.get('view').id;
  if (id === current) return;
  const before = meta.views.find((v) => v.id === current)?.quantisation ?? null;
  const after = meta.views.find((v) => v.id === id)?.quantisation ?? null;
  store.setCurrentView(id);
  emit(from, 'tessera-viewswitch', {from: current, to: id, sameFrame: sameFrame(before, after)});
}
