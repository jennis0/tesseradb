import type {ArtifactDetail, ClauseVerb, Count, FilterExpr, ItemDetail, Masked, RegionProjection, Refusal, SelectionShape} from '@tesseradb/client';
import type {PanelState} from './states.js';

/** A selection shape as an event carries it: an artifact's `tessera_id` as a decimal string. */
export type SelectionShapeDetail = Exclude<SelectionShape, {kind: 'artifact'}> | {kind: 'artifact'; id: string; outside?: boolean};

/**
 * Every event the elements fire, by name, with its `detail`. Each bubbles and is composed, so a host
 * listens on any ancestor, including one outside `<tessera-explorer>`. A `tessera_id` crosses as a
 * decimal string, since it is a 64-bit integer.
 */
export type TesseraEventDetails = {
  /** The panel state moved from `from` to `to`; `from` is null on the first render. */
  'tessera-statechange': {from: PanelState | null; to: PanelState};
  /** The session expired; `refusal` is the refusal that ended it, where one did. */
  'tessera-expired': {refusal: Refusal | null};
  /**
   * The camera moved. `bbox` is `[x0, y0, x1, y1]` in the view's data coordinates, `zoom` the
   * map's zoom (0 when the 512-unit world fills 512 pixels, one more per doubling), and `width` and
   * `height` the canvas in CSS pixels.
   */
  'tessera-viewchange': {bbox: [number, number, number, number]; zoom: number; width: number; height: number};
  /**
   * A point was picked: `id` is its `tessera_id`. Fired on the click without `record`, and again
   * with the item's record once it arrives.
   */
  'tessera-pick': {id: string; record?: ItemDetail};
  /** The pointer is over the point `id`, at `x`, `y` in canvas pixels. */
  'tessera-hover': {id: string; x: number; y: number};
  /**
   * The artifact `id` was opened and its drill-down arrived, as `detail`, with `maskedCount` as a
   * decimal string.
   */
  'tessera-artifactopen': {id: string; detail: Omit<ArtifactDetail, 'maskedCount'> & {maskedCount: string}};
  /** The artifact `id` of layer `layer` was chosen from a list. */
  'tessera-artifactselect': {id: string; layer: string};
  /** Fit was asked for the artifact `id`. `<tessera-explorer>` fits its map to it. */
  'tessera-artifactfit': {id: string};
  /**
   * The selection changed. `shape` is the new shape in data coordinates, or null where the
   * selection was cleared. `status` is `loading` as the shape is sent and `cleared` as it is
   * dropped; once the server answers, it is the region's `shown` or `refused`, and `visible`,
   * `matched`, `served` and `verdict` carry the region's counts and whether they are exact for the
   * shape.
   */
  'tessera-selectchange': {
    shape: SelectionShapeDetail | null;
    status?: RegionProjection['status'] | 'cleared';
    visible?: Masked | null;
    matched?: Masked;
    served?: Count;
    verdict?: RegionProjection['verdict'];
  };
  /** The layers chosen are now `layers`; the store also draws the layers each depends on, which the list leaves out. */
  'tessera-layerchange': {layers: string[]};
  /** The points are now coloured by `colourBy`: a column, `cluster:<layer>`, or null for one colour. */
  'tessera-colourchange': {colourBy: string | null};
  /** The level to colour and label at is now `level`; null is the level drawn by default. */
  'tessera-levelchange': {level: number | null};
  /**
   * A filter changed. `column` is the column whose control changed, or null for Clear all. After an
   * edit in a control, `expr` is the expression the controls now compose, null for none. Where a
   * chip was removed or Clear all pressed, `expr` is null. Where a clause moved between `filter`
   * and `highlight`, `verb` is its new position and `expr` is absent.
   */
  'tessera-filterchange': {column: string | null; expr?: FilterExpr | null; verb?: ClauseVerb};
  /**
   * Open was pressed on the item `id`: `fields` is its record's fields and `externalId` its
   * external id, or null where it has none.
   */
  'tessera-open': {id: string; fields: Record<string, unknown>; externalId: string | null};
  /**
   * The view changed from `from` to `to`. `sameFrame` is true where both views are quantised
   * against one extent, so the camera keeps its place; otherwise the map refits.
   */
  'tessera-viewswitch': {from: string; to: string; sameFrame: boolean};
  /**
   * Follow an item into the view `view`, where it sits at `x`, `y` in that view's data coordinates.
   * `<tessera-explorer>` switches to the view and centres the map there.
   */
  'tessera-viewfollow': {view: string; x: number; y: number};
  /** A card's close button was pressed: `what` says which card. */
  'tessera-close': {what: 'item' | 'artifact'};
  /**
   * A `member_of` clause on the artifact `id` of layer `layer` was put on (`on` true) or taken off,
   * in the position `verb`, selecting its members or, where `outside` is true, everything else.
   */
  'tessera-clausechange': {id: string; layer: string; outside: boolean; verb: ClauseVerb; on: boolean};
};

/** Every event the elements fire, as the `CustomEvent` a listener receives. */
export type TesseraEventMap = {[K in keyof TesseraEventDetails]: CustomEvent<TesseraEventDetails[K]>};

declare global {
  interface HTMLElementEventMap extends TesseraEventMap {}
}
