import type {ArtifactDetail, ClauseVerb, Count, FilterExpr, ItemDetail, Masked, PaletteName, RegionProjection, Refusal, SelectionShape} from '@mosaicajs/client';
import type {CategoryPaletteName, DensityColours, DensityMode, DensityScale, RampName, RampScale, SizeScale} from '@mosaicajs/deck';
import type {PanelState} from './states.js';

/** A selection shape as an event carries it: an artifact's `tessera_id` as a decimal string. */
export type SelectionShapeDetail = Exclude<SelectionShape, {kind: 'artifact'}> | {kind: 'artifact'; id: string; outside?: boolean};

/**
 * Every event the elements fire, by name, with its `detail`. Each bubbles and is composed, so a
 * host listens on any ancestor, including one outside `<mosaica-explorer>`. A `tessera_id` crosses
 * as a decimal string, since it is a 64-bit integer.
 */
export type MosaicaEventDetails = {
  /** The panel state moved from `from` to `to`; `from` is null on the first render. */
  'mosaica-statechange': {from: PanelState | null; to: PanelState};
  /** The session expired; `refusal` is the refusal that ended it, where one did. */
  'mosaica-expired': {refusal: Refusal | null};
  /**
   * The camera moved. `bbox` is `[x0, y0, x1, y1]` in the view's data coordinates, `zoom` the
   * map's zoom (0 when the 512-unit world fills 512 pixels, one more per doubling), and `width` and
   * `height` the canvas in CSS pixels.
   */
  'mosaica-viewchange': {bbox: [number, number, number, number]; zoom: number; width: number; height: number};
  /**
   * A point was picked: `id` is its `tessera_id`. Fired on the click without `record`, and again
   * with the item's record once it arrives.
   */
  'mosaica-pick': {id: string; record?: ItemDetail};
  /** A click on the map found no point and no artifact. */
  'mosaica-miss': Record<string, never>;
  /** The pointer is over the point `id`, at `x`, `y` in canvas pixels. */
  'mosaica-hover': {id: string; x: number; y: number};
  /**
   * The artifact `id` was opened and its drill-down arrived, as `detail`, with `maskedCount` as a
   * decimal string.
   */
  'mosaica-artifactopen': {id: string; detail: Omit<ArtifactDetail, 'maskedCount'> & {maskedCount: string}};
  /** Fit was asked for the artifact `id`. `<mosaica-explorer>` fits its map to it. */
  'mosaica-artifactfit': {id: string};
  /**
   * The selection changed. `shape` is the new shape in data coordinates, or null where the
   * selection was cleared. `status` is `loading` as the shape is sent and `cleared` as it is
   * dropped; once the server answers, it is the region's `shown` or `refused`, and `visible`,
   * `matched`, `served` and `verdict` carry the region's counts and whether they are exact for the
   * shape.
   */
  'mosaica-selectchange': {
    shape: SelectionShapeDetail | null;
    status?: RegionProjection['status'] | 'cleared';
    visible?: Masked | null;
    matched?: Masked;
    served?: Count;
    verdict?: RegionProjection['verdict'];
  };
  /** The layers chosen are now `layers`; the store also draws the layers each depends on, which the list leaves out. */
  'mosaica-layerchange': {layers: string[]};
  /** The points are now coloured by `colourBy`: a column, `cluster:<layer>`, or null for one colour. */
  'mosaica-colourchange': {colourBy: string | null};
  /**
   * The size choices changed in the explorer's Layers popover; the detail is all four as they now
   * stand. `sizeBy` is the number column the points are sized by, or null for one size; `min` and
   * `max` are the radii in pixels of its smallest and largest value, and `scale` how values are
   * placed between them.
   */
  'mosaica-sizechange': {sizeBy: string | null; min: number; max: number; scale: SizeScale};
  /** A field card's fold button folded it to one line (`folded` true) or opened it. */
  'mosaica-fold': {field: string; folded: boolean};
  /** Most points was let go: the store now aims to draw `budget` marks on screen. */
  'mosaica-budgetchange': {budget: number};
  /** Most clusters was let go: a `nested` or `dag` layer is now cut to at most `budget` clusters. */
  'mosaica-clusterbudgetchange': {budget: number};
  /** The level to colour and label at is now `level`; null is the level drawn by default. */
  'mosaica-levelchange': {level: number | null};
  /**
   * Colours were chosen for values of the column `column`, in one action: one value's in a colour
   * picker, the selected values' in Edit colours, or every value's by Reset to palette there. Each
   * change names a value (a category key) and its colour as `#rrggbb`, or `null` where its chosen
   * colour was reset so it takes its palette colour again. The map already draws them; a host that
   * keeps the choices sets them back through the map's or the explorer's `valueColours`.
   */
  'mosaica-valuecolour': {column: string; changes: {value: string; colour: string | null}[]};
  /**
   * Colours were chosen for clusters of the layer `layer`, in one action, as for
   * `mosaica-valuecolour`. Each change names a cluster by its `tessera_id` and its colour as
   * `#rrggbb`, or `null` where its chosen colour was reset so it takes its palette colour again.
   * The map already draws them; a host that keeps the choices sets them back through the map's or
   * the explorer's `clusterColours`.
   */
  'mosaica-clustercolour': {layer: string; changes: {tesseraId: string; colour: string | null}[]};
  /** The palette, the ramp, the ramp's scale or its direction was chosen in the explorer's Colour section; the detail is all four as they now stand. */
  'mosaica-palettechange': {palette: CategoryPaletteName; ramp: RampName; scale: RampScale; reverse: boolean};
  /**
   * A palette was chosen for a layer's clusters in the explorer's Colour section, and set on the
   * store (`Store.setPalette`).
   */
  'mosaica-clusterpalettechange': {palette: PaletteName};
  /**
   * A display setting was changed in the explorer's Layers popover; the detail is every setting as
   * it now stands. `radius` and `pointOpacity` are null where the map sizes and fades the points by
   * how many are drawn, and `densityColours` is null where the map chooses. `densityResolution` is
   * the cell size chosen on the Resolution slider, in CSS pixels, and `densityScale` the scale
   * density's colours follow, `linear` or `log`.
   */
  'mosaica-displaychange': {
    points: boolean;
    radius: number | null;
    pointOpacity: number | null;
    density: DensityMode;
    densityColours: DensityColours | null;
    densityStrength: number;
    densityResolution: number;
    densityScale: DensityScale;
  };
  /**
   * A filter changed. `column` is the column whose control changed, or null for Clear all and for
   * clearing the highlight. After an edit in a control or on a field card, `expr` is the expression
   * the edited position now composes, null for none, and `verb` names that position (`filter` where
   * absent). Where a chip was removed, `verb` is the position it was in and `expr` is null; after
   * Clear all, `expr` is null, and after clearing the highlight `verb` is `highlight`.
   */
  'mosaica-filterchange': {column: string | null; expr?: FilterExpr | null; verb?: ClauseVerb};
  /** A column's chip was pressed under `chips-only`: its card is to be shown; `verb` is the chip's position. */
  'mosaica-chipopen': {column: string; verb: ClauseVerb};
  /** Open was pressed on the item `id`: `fields` is its record's fields. */
  'mosaica-open': {id: string; fields: Record<string, unknown>};
  /**
   * The view changed from `from` to `to`. `sameFrame` is true where both views are quantised
   * against one extent, so the camera keeps its place; otherwise the map refits.
   */
  'mosaica-viewswitch': {from: string; to: string; sameFrame: boolean};
  /**
   * Follow an item into the view `view`, where it sits at `x`, `y` in that view's data coordinates.
   * `<mosaica-explorer>` switches to the view and centres the map there.
   */
  'mosaica-viewfollow': {view: string; x: number; y: number};
  /** A card's close button was pressed: `what` says which card. */
  'mosaica-close': {what: 'item' | 'artifact'};
  /**
   * A `member_of` clause on the artifact `id` of layer `layer` was put on (`on` true) or taken off,
   * in the position `verb`, selecting its members or, where `outside` is true, everything else.
   */
  'mosaica-clausechange': {id: string; layer: string; outside: boolean; verb: ClauseVerb; on: boolean};
};

/** Every event the elements fire, as the `CustomEvent` a listener receives. */
export type MosaicaEventMap = {[K in keyof MosaicaEventDetails]: CustomEvent<MosaicaEventDetails[K]>};

declare global {
  interface HTMLElementEventMap extends MosaicaEventMap {}
}
