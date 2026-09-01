import {createComponent, type EventName} from '@lit/react';
import * as React from 'react';
import type {ArtifactDetail, FilterExpr, ItemDetail, Refusal, SelectionShape} from '@tesseradb/client';
import {
  TesseraArtifactCard as ArtifactCardElement,
  TesseraArtifactList as ArtifactListElement,
  TesseraCount as CountElement,
  TesseraExplorer as ExplorerElement,
  TesseraFilter as FilterElement,
  TesseraFilterPanel as FilterPanelElement,
  TesseraItemCard as ItemCardElement,
  TesseraKeyPicker as KeyPickerElement,
  TesseraLayerPicker as LayerPickerElement,
  TesseraLegend as LegendElement,
  TesseraMap as MapElement,
  TesseraSelection as SelectionElement,
  TesseraStatus as StatusElement,
  TesseraStore as StoreElement,
  TesseraViewPicker as ViewPickerElement
} from '@tesseradb/components';

/**
 * `@tesseradb/react/components` — every `tessera-*` element as a React component with typed
 * props and events (design §5.9). Importing this entry imports `@tesseradb/components`, which
 * defines the elements on import and pulls in Lit and, through the map, deck.gl; a host that
 * wants only the hooks imports the package root instead.
 *
 * `@lit/react` sets a prop that names a property on the element as the **property**, never as
 * an attribute — which is what an object value (`store`, `count`, `item`, `authorise`) needs and
 * what React 18 cannot do on a custom element by itself. React 19 sets properties natively, and
 * the wrappers still earn their place there for the typing: a `store` prop is a `Store`, and an
 * `onPick` prop is a handler for the event the map emits, with its detail typed. The components
 * carry the elements' names (`TesseraMap` is `<tessera-map>`); the element classes are exported
 * as types with an `Element` suffix, for a ref.
 *
 * Event details are the elements' own (`base.ts`'s `emit`): ids cross as decimal strings.
 */

type Detail<T> = CustomEvent<T>;

/** The events each element emits — one entry per `emit` site in `@tesseradb/components`. */
export type TesseraEvents = {
  'tessera-statechange': Detail<{from: string; to: string}>;
  'tessera-expired': Detail<{refusal: Refusal | null}>;
  'tessera-viewchange': Detail<{bbox: [number, number, number, number]; zoom: number; width: number; height: number}>;
  'tessera-pick': Detail<{id: string; record?: ItemDetail}>;
  'tessera-hover': Detail<{id: string; x: number; y: number}>;
  'tessera-artifactopen': Detail<{id: string; detail: ArtifactDetail}>;
  'tessera-artifactselect': Detail<{id: string; layer: string}>;
  'tessera-artifactfit': Detail<{id: string}>;
  'tessera-selectchange': Detail<{shape: SelectionShape | null; status?: string}>;
  'tessera-layerchange': Detail<{layers: string[]}>;
  'tessera-colourchange': Detail<{colourBy: string | null}>;
  'tessera-filterchange': Detail<{column: string | null; expr: FilterExpr | null}>;
  'tessera-open': Detail<{id: string; fields: Record<string, unknown>; externalId: string | null}>;
  'tessera-viewswitch': Detail<{from: string; to: string; sameFrame: boolean}>;
  'tessera-viewfollow': Detail<{view: string; x: number; y: number}>;
};

const ev = <K extends keyof TesseraEvents>(name: K) => name as EventName<TesseraEvents[K]>;

const wrap = <E extends HTMLElement, Ev extends Record<string, EventName>>(tagName: string, elementClass: new () => E, events: Ev) =>
  createComponent({react: React, tagName, elementClass, events, displayName: elementClass.name});

export const TesseraStore = wrap('tessera-store', StoreElement, {});
export const TesseraExplorer = wrap('tessera-explorer', ExplorerElement, {
  onPick: ev('tessera-pick'),
  onHover: ev('tessera-hover'),
  onViewChange: ev('tessera-viewchange'),
  onSelectChange: ev('tessera-selectchange'),
  onArtifactOpen: ev('tessera-artifactopen'),
  onArtifactSelect: ev('tessera-artifactselect'),
  onLayerChange: ev('tessera-layerchange'),
  onColourChange: ev('tessera-colourchange'),
  onFilterChange: ev('tessera-filterchange'),
  onStateChange: ev('tessera-statechange'),
  onExpired: ev('tessera-expired'),
  onOpen: ev('tessera-open'),
  onViewSwitch: ev('tessera-viewswitch'),
  onViewFollow: ev('tessera-viewfollow')
});
export const TesseraMap = wrap('tessera-map', MapElement, {
  onPick: ev('tessera-pick'),
  onHover: ev('tessera-hover'),
  onViewChange: ev('tessera-viewchange'),
  onSelectChange: ev('tessera-selectchange'),
  onArtifactOpen: ev('tessera-artifactopen'),
  onLayerChange: ev('tessera-layerchange')
});
export const TesseraStatus = wrap('tessera-status', StatusElement, {onStateChange: ev('tessera-statechange'), onExpired: ev('tessera-expired')});
export const TesseraCount = wrap('tessera-count', CountElement, {});
export const TesseraItemCard = wrap('tessera-item-card', ItemCardElement, {onOpen: ev('tessera-open')});
export const TesseraFilter = wrap('tessera-filter', FilterElement, {onFilterChange: ev('tessera-filterchange')});
export const TesseraFilterPanel = wrap('tessera-filter-panel', FilterPanelElement, {onFilterChange: ev('tessera-filterchange')});
export const TesseraSelection = wrap('tessera-selection', SelectionElement, {onSelectChange: ev('tessera-selectchange')});
export const TesseraLayerPicker = wrap('tessera-layer-picker', LayerPickerElement, {onLayerChange: ev('tessera-layerchange')});
export const TesseraViewPicker = wrap('tessera-view-picker', ViewPickerElement, {onViewSwitch: ev('tessera-viewswitch')});
export const TesseraKeyPicker = wrap('tessera-key-picker', KeyPickerElement, {onViewSwitch: ev('tessera-viewswitch')});
export const TesseraArtifactList = wrap('tessera-artifact-list', ArtifactListElement, {onArtifactSelect: ev('tessera-artifactselect')});
export const TesseraArtifactCard = wrap('tessera-artifact-card', ArtifactCardElement, {onArtifactFit: ev('tessera-artifactfit')});
export const TesseraLegend = wrap('tessera-legend', LegendElement, {onColourChange: ev('tessera-colourchange')});

export type {
  ArtifactCardElement,
  ArtifactListElement,
  CountElement,
  ExplorerElement,
  FilterElement,
  FilterPanelElement,
  ItemCardElement,
  KeyPickerElement,
  LayerPickerElement,
  LegendElement,
  MapElement,
  SelectionElement,
  StatusElement,
  StoreElement,
  ViewPickerElement
};
