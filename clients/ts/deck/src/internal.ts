/**
 * `@tesseradb/deck/internal`: what `@tesseradb/components` uses of this package beyond the root
 * entry. It is not a public API, and its exports change with the components.
 */
export {artifactName, attachedTopics, clusterLayerOf, contourShapes, displayName, encodingOf, encodingSignature} from './layer.js';
export {MarkSlab} from './slab.js';
export {artifactOfMark} from './pick.js';
export {UNMAPPED, colourOfFraction, colourOfRank, css, paletteValues} from './colour.js';
export {hoverAt, type ContourShape} from './contours.js';
