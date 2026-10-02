/**
 * A deck.gl layer that draws a Tessera store, for a host that builds its own `Deck`.
 * {@link TesseraLayer} draws the marks, density, the artifacts' names and outlines and the
 * selection. {@link CATEGORY_PALETTES} and {@link RAMPS} are the named colour sets a
 * {@link Colouring} chooses from, and {@link Sizing} the sizes a number column sizes marks
 * between. {@link viewInputOf} turns the host's camera into a view for the store,
 * {@link DensityCounter} keeps the counts density is drawn from, and {@link resolvePick} reads a
 * pick.
 *
 * @module @tesseradb/deck
 */
export {TesseraLayer, type TesseraLayerProps, type LayerTimings} from './layer.js';
export {
  CATEGORY_PALETTES,
  DEFAULT_COLOURING,
  RAMPS,
  type CategoryPalette,
  type CategoryPaletteName,
  type Colouring,
  type Ramp,
  type RampName,
  type RampScale,
  type Rgb
} from './colour.js';
export {DEFAULT_SIZING, type SizeScale, type Sizing} from './size.js';
export {DEFAULT_DENSITY_SCALE, densityCountAt, densityPosition, type DensityCell, type DensityColours, type DensityCounts, type DensityMode, type DensityScale} from './density.js';
export {
  DEFAULT_DENSITY_CELL_PX,
  DENSITY_CELL_SIZES,
  DENSITY_SETTLE_MS,
  DensityCounter,
  cellDepth,
  nearestStop,
  resolutionStops,
  type DensityCamera,
  type DensitySettings,
  type ResolutionStop
} from './density-counter.js';
export {viewInputOf, type OrthographicCamera} from './camera.js';
export {resolvePick, type Picked, type PickInfo} from './pick.js';
