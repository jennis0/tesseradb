export {TesseraLayer, artifactName, hasText, clusterLayerOf, encodingOf, encodingSignature, outlineOf, contourShapes, focusOutlines, labelBudget, LABEL_CANDIDATE_CEILING, labelCandidates, frontier, attachedTopics, displayName, type TesseraLayerProps, type LayerTimings, type Outline, type OutlineSource, type OutlineDatum, type OutlineOptions, type ContourOptions, type LabelText} from './layer.js';
export {LookupTexture, buildLut, patchLut, lutRows, dimmed, LUT_WIDTH, LUT_SHIFT, type LutInputs} from './lut.js';
export {MarksLayer, type MarksLayerProps} from './marks-layer.js';
export {placeLabels, labelSize, LABEL_SIZE_MIN, LABEL_SIZE_MAX, wrapLabel, MAX_DISPLACEMENT, MAX_LABEL_LINE_CHARS, MAX_LABEL_LINES, LABEL_LINE_HEIGHT, type LabelCandidate, type PlacedLabel} from './labels.js';
export {MarkSlab, type SlabDraw, type SlabLayer, type GpuSlab} from './slab.js';
export {
  UNIFORM,
  UNMAPPED,
  PALETTE_SIZE,
  buildColourAttribute,
  colourOfFraction,
  colourOfRank,
  css,
  formatScalar,
  paletteValues,
  writeColours,
  type Encoding
} from './colour.js';
export {binDensity, filterDensity, DENSITY_SUPERSAMPLE, WASH_HUE, type DensityImage} from './density.js';
export {resolvePick, artifactOfMark, type Picked, type PickInfo} from './pick.js';
export {materialiseStandIn, type StandInBuffers} from './assemble.js';
export {markStyle, deckOpacity, ANTIALIAS_ABOVE_PX, type MarkStyle} from './marks-style.js';
export {
  distanceToRing,
  hoverAt,
  pointInRing,
  ringWithin,
  shapeBbox,
  shapeContains,
  shapeDistance,
  signedArea2,
  smoothRing,
  type ContourShape,
  type Ring
} from './contours.js';
