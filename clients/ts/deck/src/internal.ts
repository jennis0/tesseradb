/**
 * `@mosaicajs/deck/internal`: what `@mosaicajs/components` uses of this package beyond the root
 * entry. It is not a public API, and its exports change with the components.
 */
export {clusterLayerOf, contourShapes, encodingOf, type MosaicaLayerInternalProps} from './layer.js';
export {MarkSlab} from './slab.js';
export {artifactOfMark} from './pick.js';
export {UNMAPPED, colourOfFraction, colourOfRank, css, encodingSignature, fractionOf, hexOf, lighter, paletteValues, rampAt, rgbOfHex, valueAtFraction} from './colour.js';
export {DENSITY_COLOUR_TITLES, densityStops, drawnCells, maxCount} from './density.js';
export {drawnSizing, hollowRadius, radiusAt, sizeEncodingOf, sizeFraction, valueAtSize, writeSizes, type SizeEncoding} from './size.js';
export {hoverAt, type ContourShape} from './contours.js';
