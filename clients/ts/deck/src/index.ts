export {TesseraLayer, artifactName, hasText, clusterLayerOf, encodingOf, encodingSignature, outlineOf, outlineData, outlinePixels, MIN_OUTLINE_PX, isFlatLayer, labelBudget, labelCandidates, servedDepths, heightsBelow, smoothClosed, attachedTopics, displayName, type TesseraLayerProps, type LayerTimings, type OutlineDatum, type OutlineOptions, type LabelText} from './layer.js';
export {LookupTexture, buildLut, dimmed, LUT_WIDTH, LUT_SHIFT, type LutInputs} from './lut.js';
export {MarksLayer, type MarksLayerProps} from './marks-layer.js';
export {placeLabels, labelSize, wrapLabel, MAX_DISPLACEMENT, MAX_LABEL_LINE_CHARS, MAX_LABEL_LINES, LABEL_LINE_HEIGHT, type LabelCandidate, type PlacedLabel} from './labels.js';
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
export {markStyle, deckOpacity, type MarkStyle} from './marks-style.js';
