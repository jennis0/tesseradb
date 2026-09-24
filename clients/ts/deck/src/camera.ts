import {type Store, type ViewInput} from '@tesseradb/client';
import {worldBbox} from '@tesseradb/client/internal';

/** The part of a deck.gl `OrthographicView` view state the conversion reads. */
export type OrthographicCamera = {target: readonly number[]; zoom: number};

/**
 * What `store.setView` takes for a deck.gl `OrthographicView` camera over the 512-unit world:
 * the canvas's world bbox, clamped to the world and converted to data coordinates through the
 * store's frame, and the canvas size in CSS pixels. Null before `meta` has arrived, when the
 * store has no frame to convert against.
 */
export function viewInputOf(store: Pick<Store, 'frame' | 'dataXY'>, camera: OrthographicCamera, width: number, height: number): ViewInput | null {
  if (!store.frame()) return null;
  const wb = worldBbox({target: [camera.target[0] ?? 0, camera.target[1] ?? 0], zoom: camera.zoom, width, height}, 1);
  const [x0, y0] = store.dataXY(wb[0], wb[1]);
  const [x1, y1] = store.dataXY(wb[2], wb[3]);
  return {bbox: [x0, y0, x1, y1], width, height};
}
