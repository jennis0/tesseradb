/**
 * A deck.gl layer that draws a Tessera store, for a host that builds its own `Deck`.
 * {@link TesseraLayer} draws the marks, the density wash, the artifacts' names and outlines and
 * the selection. {@link viewInputOf} turns the host's camera into a view for the store, and
 * {@link resolvePick} reads a pick.
 *
 * @module @tesseradb/deck
 */
export {TesseraLayer, type TesseraLayerProps, type LayerTimings} from './layer.js';
export {viewInputOf, type OrthographicCamera} from './camera.js';
export {resolvePick, type Picked, type PickInfo} from './pick.js';
