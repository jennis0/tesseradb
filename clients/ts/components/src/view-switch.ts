import type {Meta, Quantisation, Store} from '@mosaicajs/client';
import {emit} from './base.js';

/**
 * What the view and key pickers share: the frame comparison and the switch. The picker rules are
 * in `@mosaicajs/client`, so a host drawing its own picker gets the same answers.
 */

/**
 * Whether two views are quantised against one frame, which decides whether a switch keeps the
 * camera or refits it. Compared by value: `/v1/meta` gives each view its own extent object.
 */
export function sameFrame(a: Quantisation | null, b: Quantisation | null): boolean {
  if (!a || !b) return a === b;
  return a.xMin === b.xMin && a.xMax === b.xMax && a.yMin === b.yMin && a.yMax === b.yMax;
}

/**
 * Make `id` the current view and emit `mosaica-viewswitch` `{from, to, sameFrame}`, for a host that
 * owns the URL or a basemap. The map follows the `view` projection, so a host calling
 * `setCurrentView` itself gets the same refit.
 */
export function switchView(from: HTMLElement, store: Store, meta: Meta, id: string): void {
  const current = store.get('view').id;
  if (id === current) return;
  const before = meta.views.find((v) => v.id === current)?.quantisation ?? null;
  const after = meta.views.find((v) => v.id === id)?.quantisation ?? null;
  store.setCurrentView(id);
  emit(from, 'mosaica-viewswitch', {from: current, to: id, sameFrame: sameFrame(before, after)});
}
