/**
 * A deck.gl layer that draws a Tessera store, for a host that builds its own `Deck`.
 * {@link TesseraLayer} draws the marks, density, the artifacts' names and outlines and the
 * selection. {@link CATEGORY_PALETTES} and {@link RAMPS} are the named colour sets a
 * {@link Colouring} chooses from, and {@link Sizing} the sizes a number column sizes marks
 * between. {@link viewInputOf} turns the host's camera into a view for the store, and
 * {@link resolvePick} reads a pick.
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
export type {DensityColours, DensityMode} from './density.js';
export {viewInputOf, type OrthographicCamera} from './camera.js';
export {resolvePick, type Picked, type PickInfo} from './pick.js';
