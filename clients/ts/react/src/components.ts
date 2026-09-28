/**
 * Every `tessera-*` element as a React component with typed props and event handlers, made with
 * `@lit/react`. `TesseraMap` wraps `<tessera-map>`, and so on. A prop that names an element
 * property is set as that property, so an object such as `store` reaches the element under React
 * 18, which sets a custom element's props as attributes. Each element's events are handler props,
 * such as `onPick` for `tessera-pick`. The element classes are exported as types with an `Element`
 * suffix, such as `MapElement`, for typing a ref. Each element's attributes, properties, events,
 * slots and parts are on its page in the Components reference, which each wrapper's type links to.
 *
 * `TesseraStore`, `TesseraMap` and `TesseraExplorer` read their `authorise` prop through a ref, so
 * an inline function does not build a new store on every render. To show the map to another
 * viewer, change `token` or `viewerUrl`, or give the component a new `key`.
 *
 * This entry imports `@tesseradb/components`, and with it Lit and deck.gl. The hooks are in the
 * package root, `@tesseradb/react`, which imports neither.
 *
 * @module @tesseradb/react/components
 */
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
import type {TokenSupplier} from '@tesseradb/client';


/**
 * The events the elements emit, by name, each a `CustomEvent` with its detail: `TesseraEventMap`
 * from `@tesseradb/components`.
 */
export type TesseraEvents = TesseraEventMap;

const ev = <K extends keyof TesseraEvents>(name: K) => name as EventName<TesseraEvents[K]>;

const wrap = <E extends HTMLElement, Ev extends Record<string, EventName>>(tagName: string, elementClass: new () => E, events: Ev) =>
  createComponent({react: React, tagName, elementClass, events, displayName: elementClass.name});

/**
 * Hands the element one function per mount in place of the `authorise` prop, and calls the latest
 * prop through a ref, as `useTesseraStore` does. An element builds a new store when its `authorise`
 * function changes, so an inline arrow passed straight through would build one on every render.
 * The store is built again when `authorise` is given or removed; to show another viewer, change
 * `token`, `viewerUrl` or the component's `key`.
 */
const withStableAuthorise = <C extends React.ForwardRefExoticComponent<any>>(Inner: C): C => {
  const Outer = React.forwardRef<unknown, {authorise?: TokenSupplier | null}>((props, ref) => {
    const latest = React.useRef(props.authorise);
    latest.current = props.authorise;
    const given = props.authorise != null;
    const authorise = React.useMemo(() => (given ? () => latest.current!() : null), [given]);
    return React.createElement(Inner, {...props, authorise, ref});
  });
  Outer.displayName = Inner.displayName;
  return Outer as unknown as C;
};

/** The handler props for events a map emits, which the explorer also carries. */
const mapEvents = {
  onPick: ev('tessera-pick'),
  onHover: ev('tessera-hover'),
  onViewChange: ev('tessera-viewchange'),
  onSelectChange: ev('tessera-selectchange'),
  onArtifactOpen: ev('tessera-artifactopen'),
  onLayerChange: ev('tessera-layerchange')
};

/** `<tessera-store>` as a React component. It has no event props. */
export const TesseraStore = withStableAuthorise(wrap('tessera-store', StoreElement, {}));
/**
 * `<tessera-explorer>` as a React component. It has a handler prop for every event, since the
 * elements inside it emit them and each event bubbles out of it: `onPick`, `onHover`,
 * `onViewChange`, `onSelectChange`, `onArtifactOpen`, `onLayerChange`, `onArtifactSelect`,
 * `onArtifactFit`, `onColourChange`, `onLevelChange`, `onFilterChange`, `onStateChange`,
 * `onExpired`, `onOpen`, `onClose`, `onClauseChange`, `onViewSwitch` and `onViewFollow`, each for
 * the `tessera-` event of the same name in lower case.
 */
export const TesseraExplorer = withStableAuthorise(wrap('tessera-explorer', ExplorerElement, {
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
}));
/**
 * `<tessera-map>` as a React component. Event props: `onPick` (`tessera-pick`), `onHover`
 * (`tessera-hover`), `onViewChange` (`tessera-viewchange`), `onSelectChange`
 * (`tessera-selectchange`), `onArtifactOpen` (`tessera-artifactopen`) and `onLayerChange`
 * (`tessera-layerchange`).
 */
export const TesseraMap = withStableAuthorise(wrap('tessera-map', MapElement, mapEvents));
/** `<tessera-status>` as a React component. Event props: `onStateChange` (`tessera-statechange`) and `onExpired` (`tessera-expired`). */
export const TesseraStatus = wrap('tessera-status', StatusElement, {onStateChange: ev('tessera-statechange'), onExpired: ev('tessera-expired')});
/** `<tessera-count>` as a React component. It has no event props. */
export const TesseraCount = wrap('tessera-count', CountElement, {});
/**
 * `<tessera-item-card>` as a React component. Event props: `onOpen` (`tessera-open`), `onClose`
 * (`tessera-close`) and `onViewFollow` (`tessera-viewfollow`).
 */
export const TesseraItemCard = wrap('tessera-item-card', ItemCardElement, {onOpen: ev('tessera-open'), onClose: ev('tessera-close'), onViewFollow: ev('tessera-viewfollow')});
/** `<tessera-filter>` as a React component. Event prop: `onFilterChange` (`tessera-filterchange`). */
export const TesseraFilter = wrap('tessera-filter', FilterElement, {onFilterChange: ev('tessera-filterchange')});
/** `<tessera-filter-panel>` as a React component. Event prop: `onFilterChange` (`tessera-filterchange`). */
export const TesseraFilterPanel = wrap('tessera-filter-panel', FilterPanelElement, {onFilterChange: ev('tessera-filterchange')});
/** `<tessera-selection>` as a React component. Event prop: `onSelectChange` (`tessera-selectchange`). */
export const TesseraSelection = wrap('tessera-selection', SelectionElement, {onSelectChange: ev('tessera-selectchange')});
/** `<tessera-layer-picker>` as a React component. Event prop: `onLayerChange` (`tessera-layerchange`). */
export const TesseraLayerPicker = wrap('tessera-layer-picker', LayerPickerElement, {onLayerChange: ev('tessera-layerchange')});
/** `<tessera-view-picker>` as a React component. Event prop: `onViewSwitch` (`tessera-viewswitch`). */
export const TesseraViewPicker = wrap('tessera-view-picker', ViewPickerElement, {onViewSwitch: ev('tessera-viewswitch')});
/** `<tessera-key-picker>` as a React component. Event prop: `onViewSwitch` (`tessera-viewswitch`). */
export const TesseraKeyPicker = wrap('tessera-key-picker', KeyPickerElement, {onViewSwitch: ev('tessera-viewswitch')});
/**
 * `<tessera-artifact-list>` as a React component. Event prop: `onArtifactSelect`
 * (`tessera-artifactselect`).
 */
export const TesseraArtifactList = wrap('tessera-artifact-list', ArtifactListElement, {onArtifactSelect: ev('tessera-artifactselect')});
/**
 * `<tessera-artifact-card>` as a React component. Event props: `onArtifactFit`
 * (`tessera-artifactfit`), `onClauseChange` (`tessera-clausechange`) and `onClose`
 * (`tessera-close`).
 */
export const TesseraArtifactCard = wrap('tessera-artifact-card', ArtifactCardElement, {
  onArtifactFit: ev('tessera-artifactfit'),
  onClauseChange: ev('tessera-clausechange'),
  onClose: ev('tessera-close')
});
/**
 * `<tessera-legend>` as a React component. Event props: `onColourChange` (`tessera-colourchange`),
 * `onLevelChange` (`tessera-levelchange`) and `onLayerChange` (`tessera-layerchange`).
 */
export const TesseraLegend = wrap('tessera-legend', LegendElement, {
  onColourChange: ev('tessera-colourchange'),
  onLevelChange: ev('tessera-levelchange'),
  onLayerChange: ev('tessera-layerchange')
});
/**
 * `<tessera-hierarchy>` as a React component. Event props: `onClauseChange` (`tessera-clausechange`)
 * and `onArtifactFit` (`tessera-artifactfit`).
 */
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
