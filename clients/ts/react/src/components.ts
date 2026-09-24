import {createComponent, type EventName} from '@lit/react';
import * as React from 'react';
import {
  TesseraArtifactCard as ArtifactCardElement,
  TesseraArtifactList as ArtifactListElement,
  TesseraCount as CountElement,
  TesseraExplorer as ExplorerElement,
  TesseraFilter as FilterElement,
  TesseraFilterPanel as FilterPanelElement,
  TesseraHierarchy as HierarchyElement,
  TesseraItemCard as ItemCardElement,
  TesseraKeyPicker as KeyPickerElement,
  TesseraLayerPicker as LayerPickerElement,
  TesseraLegend as LegendElement,
  TesseraMap as MapElement,
  TesseraSelection as SelectionElement,
  TesseraStatus as StatusElement,
  TesseraStore as StoreElement,
  TesseraViewPicker as ViewPickerElement,
  type TesseraEventMap
} from '@tesseradb/components';

/**
 * `@tesseradb/react/components`: every `tessera-*` element as a React component with typed props
 * and events. This entry imports `@tesseradb/components`, and with it Lit and deck.gl; a host that
 * wants only the hooks imports the package root.
 *
 * `@lit/react` sets a prop that names an element property as that property, not an attribute,
 * which object values such as `store` need and React 18 does not do for custom elements. Under
 * React 19 the wrappers still type the props and event handlers. `TesseraMap` wraps
 * `<tessera-map>`, and so on; the element classes are exported as types with an `Element` suffix,
 * for a ref.
 */

/** The events the elements emit, as `@tesseradb/components` declares them. */
export type TesseraEvents = TesseraEventMap;

const ev = <K extends keyof TesseraEvents>(name: K) => name as EventName<TesseraEvents[K]>;

const wrap = <E extends HTMLElement, Ev extends Record<string, EventName>>(tagName: string, elementClass: new () => E, events: Ev) =>
  createComponent({react: React, tagName, elementClass, events, displayName: elementClass.name});

/** The handler props for events a map emits, which the explorer also carries. */
const mapEvents = {
  onPick: ev('tessera-pick'),
  onHover: ev('tessera-hover'),
  onViewChange: ev('tessera-viewchange'),
  onSelectChange: ev('tessera-selectchange'),
  onArtifactOpen: ev('tessera-artifactopen'),
  onLayerChange: ev('tessera-layerchange')
};

export const TesseraStore = wrap('tessera-store', StoreElement, {});
/** The explorer carries every event: its pieces emit inside it, and each event is composed. */
export const TesseraExplorer = wrap('tessera-explorer', ExplorerElement, {
  ...mapEvents,
  onArtifactSelect: ev('tessera-artifactselect'),
  onArtifactFit: ev('tessera-artifactfit'),
  onColourChange: ev('tessera-colourchange'),
  onLevelChange: ev('tessera-levelchange'),
  onFilterChange: ev('tessera-filterchange'),
  onStateChange: ev('tessera-statechange'),
  onExpired: ev('tessera-expired'),
  onOpen: ev('tessera-open'),
  onClose: ev('tessera-close'),
  onClauseChange: ev('tessera-clausechange'),
  onViewSwitch: ev('tessera-viewswitch'),
  onViewFollow: ev('tessera-viewfollow')
});
export const TesseraMap = wrap('tessera-map', MapElement, mapEvents);
export const TesseraStatus = wrap('tessera-status', StatusElement, {onStateChange: ev('tessera-statechange'), onExpired: ev('tessera-expired')});
export const TesseraCount = wrap('tessera-count', CountElement, {});
export const TesseraItemCard = wrap('tessera-item-card', ItemCardElement, {onOpen: ev('tessera-open'), onClose: ev('tessera-close'), onViewFollow: ev('tessera-viewfollow')});
export const TesseraFilter = wrap('tessera-filter', FilterElement, {onFilterChange: ev('tessera-filterchange')});
export const TesseraFilterPanel = wrap('tessera-filter-panel', FilterPanelElement, {onFilterChange: ev('tessera-filterchange')});
export const TesseraSelection = wrap('tessera-selection', SelectionElement, {onSelectChange: ev('tessera-selectchange')});
export const TesseraLayerPicker = wrap('tessera-layer-picker', LayerPickerElement, {onLayerChange: ev('tessera-layerchange')});
export const TesseraViewPicker = wrap('tessera-view-picker', ViewPickerElement, {onViewSwitch: ev('tessera-viewswitch')});
export const TesseraKeyPicker = wrap('tessera-key-picker', KeyPickerElement, {onViewSwitch: ev('tessera-viewswitch')});
export const TesseraArtifactList = wrap('tessera-artifact-list', ArtifactListElement, {onArtifactSelect: ev('tessera-artifactselect')});
export const TesseraArtifactCard = wrap('tessera-artifact-card', ArtifactCardElement, {
  onArtifactFit: ev('tessera-artifactfit'),
  onClauseChange: ev('tessera-clausechange'),
  onClose: ev('tessera-close')
});
export const TesseraLegend = wrap('tessera-legend', LegendElement, {
  onColourChange: ev('tessera-colourchange'),
  onLevelChange: ev('tessera-levelchange'),
  onLayerChange: ev('tessera-layerchange')
});
export const TesseraHierarchy = wrap('tessera-hierarchy', HierarchyElement, {onClauseChange: ev('tessera-clausechange'), onArtifactFit: ev('tessera-artifactfit')});

export type {
  ArtifactCardElement,
  ArtifactListElement,
  CountElement,
  ExplorerElement,
  FilterElement,
  FilterPanelElement,
  HierarchyElement,
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
