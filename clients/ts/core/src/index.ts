/**
 * The headless client. `createStore` returns the store a host draws from and steers.
 * `TesseraClient` calls the viewer and session routes, `Control` the operator's control plane, and
 * `RecordsRead` is a bulk read of items or artifacts. The helpers build the filter drafts and
 * member clauses the store takes, say which layers it draws and colours by, and format its counts.
 *
 * @module @tesseradb/client
 */
export {createStore, CLUSTER_PREFIX, REGION_HELD_LIMIT, type Store} from './store.js';
export type {
  ArtifactsProjection,
  FiltersProjection,
  LegendProjection,
  MarksProjection,
  ProjectionName,
  Projections,
  RegionProjection,
  ReplicaProjection,
  SelectionProjection,
  SelectionShape,
  StatusProjection,
  StoreOptions,
  TilesProjection,
  TokenSupplier,
  ViewInput,
  ViewProjection
} from './store.js';
export type {SuggestionPage, SuggestState} from './suggestions.js';
export type {Band, BandMembership} from './bands.js';
export type {ComposedTile, Composition, StandInPiece} from './compose.js';
export type {PresentedStatus, Refusal} from './presented.js';
export {subtreeOf, type ServedLineage} from './artifactChannel.js';
export {NO_ORDINAL, type ArtifactEntry, type ArtifactTable, type ArtifactTableChange} from './artifactTable.js';
export type {Domain, Ranks, ValueSample} from './encoding.js';
export type {PaletteKind, PaletteScheme, Rgba} from './palette.js';

export {formatCount, formatMasked, NO_COUNT, NO_MASKED, type Count, type FormatOptions, type Masked} from './counts.js';

export {
  activeCount,
  composeFilters,
  emptyDraft,
  isPopulated,
  textQueryOf,
  textTerms,
  withoutClause,
  type ClauseVerb,
  type ColumnDraft,
  type FilterDraft,
  type TextTerms
} from './filters.js';
export {browsableLayers, colourLayers, drawableLayers, isFilterLayer, layerClosure, layerEntries, type LayerEntry} from './layers.js';
export {artifactName} from './names.js';
export {memberKey, memberLeaf, memberOf, withMember, withMembers, withoutMember, type MemberClause} from './members.js';

export {GRID32, MAX_DEPTH, WORLD_SIZE, dataToWorldXY, gridToWorld, gridToWorldXY} from './coords.js';
export {lonLatOfCell} from './projection.js';

export {TesseraClient, TesseraError, type PartSink, type TesseraClientOptions} from './client.js';
export {inlineDecoder, setWorkerFactory, type Decoder} from './decoder.js';
export {RecordsRead} from './records.js';
export {
  Control,
  UNANSWERED,
  type Answer,
  type CallOptions,
  type ChangeItem,
  type ControlOptions,
  type RowAnswer,
  type RowOptions,
  type WriteOptions
} from './control.js';
export * from './types.js';
