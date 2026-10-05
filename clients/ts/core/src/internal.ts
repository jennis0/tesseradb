/**
 * `@tesseradb/client/internal`: what `@tesseradb/deck`, `@tesseradb/components` and the demo
 * viewer use of this package beyond the root entry. It is not a public API. Its exports change with
 * those packages, which depend on this package's exact version.
 */
export {SessionArtifactTable} from './artifactTable.js';
export {servedLineage} from './artifactChannel.js';
export {attachedTextOf} from './names.js';
export {bandsOfResult} from './bands.js';
export type {DepthChoice} from './budget.js';
export {compose, fold} from './compose.js';
export {GRID32_PER_WORLD_UNIT, mortonOfTile, tileXY} from './coords.js';
export {decodeViewport} from './decode.js';
export {workerDecoder} from './decoder.js';
export {hasValue, numericValues, rankedValues, sizesPoints} from './encoding.js';
export {artifactColours, positionalEntry, NEUTRAL} from './palette.js';
export {worldBbox} from './prefetch.js';
export {assertCompositionMatchesServed, refusalOf} from './presented.js';
export {basemapScheme} from './projection.js';
export type {TileRect} from './rects.js';
export {regionOperand, withRegion} from './region.js';
export type {ReplicaFrame} from './replica.js';
export {enterGroup, hasOneLayout, stepView, viewLabel, viewPickerEntries, viewsOfGroup} from './views.js';
