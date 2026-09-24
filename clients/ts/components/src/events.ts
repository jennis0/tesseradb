import type {ArtifactDetail, ClauseVerb, Count, FilterExpr, ItemDetail, Masked, RegionProjection, Refusal, SelectionShape} from '@tesseradb/client';
import type {PanelState} from './states.js';

/**
 * Every event the elements emit, with its detail. Each bubbles and is composed, so a host listens
 * on any ancestor. Ids cross as decimal strings.
 */

/** A selection shape as an event carries it: an artifact's id as a decimal string. */
export type SelectionShapeDetail = Exclude<SelectionShape, {kind: 'artifact'}> | {kind: 'artifact'; id: string; outside?: boolean};

export type TesseraEventDetails = {
  /** `<tessera-status>`: the panel state moved; `from` is null on the first render. */
  'tessera-statechange': {from: PanelState | null; to: PanelState};
  /** `<tessera-status>`: the session expired. */
  'tessera-expired': {refusal: Refusal | null};
  /** `<tessera-map>`: the camera moved, in data coordinates. */
  'tessera-viewchange': {bbox: [number, number, number, number]; zoom: number; width: number; height: number};
  /** `<tessera-map>`: a point was clicked, and again with its record once it arrives. */
  'tessera-pick': {id: string; record?: ItemDetail};
  'tessera-hover': {id: string; x: number; y: number};
  /** `<tessera-map>`: an artifact was opened and its drill-down arrived, its count a decimal string. */
  'tessera-artifactopen': {id: string; detail: Omit<ArtifactDetail, 'maskedCount'> & {maskedCount: string}};
  'tessera-artifactselect': {id: string; layer: string};
  'tessera-artifactfit': {id: string};
  /**
   * A selection changed: `null` clears. `status` is `loading` as it is asked and the region's own
   * once it has answered, when the counts and the verdict come with it.
   */
  'tessera-selectchange': {
    shape: SelectionShapeDetail | null;
    status?: RegionProjection['status'] | 'cleared';
    visible?: Masked | null;
    matched?: Masked;
    served?: Count;
    verdict?: RegionProjection['verdict'];
  };
  'tessera-layerchange': {layers: string[]};
  'tessera-colourchange': {colourBy: string | null};
  /** `<tessera-legend>`: the level chosen; `null` is the deepest served. */
  'tessera-levelchange': {level: number | null};
  /** A filter control changed: `expr` is the composed filter, `verb` a clause moved between positions. */
  'tessera-filterchange': {column: string | null; expr?: FilterExpr | null; verb?: ClauseVerb};
  'tessera-open': {id: string; fields: Record<string, unknown>; externalId: string | null};
  'tessera-viewswitch': {from: string; to: string; sameFrame: boolean};
  'tessera-viewfollow': {view: string; x: number; y: number};
  /** A card's close button. */
  'tessera-close': {what: 'item' | 'artifact'};
  /** A `member_of` clause on an artifact was put on or taken off. */
  'tessera-clausechange': {id: string; layer: string; outside: boolean; verb: ClauseVerb; on: boolean};
};

export type TesseraEventMap = {[K in keyof TesseraEventDetails]: CustomEvent<TesseraEventDetails[K]>};

declare global {
  interface HTMLElementEventMap extends TesseraEventMap {}
}
