/**
 * `@tesseradb/client`: the headless store a host draws from, the viewer and session client, the
 * control-plane client, the bulk reads, and the types a host reads off them.
 *
 * `@tesseradb/client/internal` holds what `@tesseradb/deck` and `@tesseradb/components` use
 * beyond this entry. It is not a public API.
 */

// The store and its projections.
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
export type {Band, BandMembership} from './bands.js';
export type {ComposedTile, Composition, StandInPiece} from './compose.js';
export type {TileRect} from './rects.js';
export type {ReplicaOptions} from './replica.js';
export type {Clock, DriverOptions} from './driver.js';
export type {FrameScheduler, PresentedStatus, Refusal} from './presented.js';
export type {ArtifactChannelState, ServedLineage} from './artifactChannel.js';
export {NO_ORDINAL, type ArtifactEntry, type ArtifactRef, type ArtifactTableChange, type SessionArtifactTable} from './artifactTable.js';
export type {Domain, Ranks} from './encoding.js';
export type {PaletteKind, PaletteScheme, Rgba} from './palette.js';

// Counts.
export {formatCount, formatMasked, NO_COUNT, NO_MASKED, type Count, type FormatOptions, type Masked} from './counts.js';

// Filters and member clauses, as `setFilters` and `setMembers` take them.
export {
  activeCount,
  composeFilters,
  emptyDraft,
  isPopulated,
  withVerb,
  type ClauseVerb,
  type ColumnDraft,
  type ColumnPredicate,
  type FilterDraft,
  type TextMode
} from './filters.js';
export {memberKey, withMember, withMembers, withoutMember, type MemberClause} from './members.js';

// Coordinates.
export {GRID32, MAX_DEPTH, WORLD_SIZE, dataToWorldXY, gridToWorld, gridToWorldXY, tileXY} from './coords.js';

// The HTTP clients and the wire types.
export {TesseraClient, TesseraError, type PartSink, type TesseraClientOptions} from './client.js';
export {inlineDecoder, setWorkerFactory, type Decoder} from './decoder.js';
export {RecordsRead} from './records.js';
export {
  Control,
  addressed,
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
