/**
 * Every `mosaica-*` element as a React component with typed props and event handlers, made with
 * `@lit/react`. `MosaicaMap` wraps `<mosaica-map>`, and so on. A prop that names an element
 * property is set as that property, so an object such as `store` reaches the element under React
 * 18, which sets a custom element's props as attributes. Each element's events are handler props,
 * such as `onPick` for `mosaica-pick`. The element classes are exported as types with an `Element`
 * suffix, such as `MapElement`, for typing a ref. Each element's attributes, properties, events,
 * slots and parts are on its page in the Components reference, which each wrapper's type links to.
 *
 * `MosaicaStore`, `MosaicaMap` and `MosaicaExplorer` read their `authorise` prop through a ref, so
 * an inline function does not build a new store on every render. To show the map to another
 * viewer, change `token` or `viewerUrl`, or give the component a new `key`.
 *
 * This entry imports `@mosaicajs/components`, and with it Lit and deck.gl. The hooks are in the
 * package root, `@mosaicajs/react`, which imports neither.
 *
 * @module @mosaicajs/react/components
 */
import {createComponent, type EventName} from '@lit/react';
import * as React from 'react';
import {
  MosaicaArtifactCard as ArtifactCardElement,
  MosaicaColourEditor as ColourEditorElement,
  MosaicaCount as CountElement,
  MosaicaExplorer as ExplorerElement,
  MosaicaFieldCard as FieldCardElement,
  MosaicaFilterPanel as FilterPanelElement,
  MosaicaHierarchy as HierarchyElement,
  MosaicaItemCard as ItemCardElement,
  MosaicaKeyPicker as KeyPickerElement,
  MosaicaLayerPicker as LayerPickerElement,
  MosaicaMap as MapElement,
  MosaicaSelection as SelectionElement,
  MosaicaStatus as StatusElement,
  MosaicaStore as StoreElement,
  MosaicaViewPicker as ViewPickerElement,
  type MosaicaEventMap
} from '@mosaicajs/components';
import type {TokenSupplier} from '@mosaicajs/client';


/**
 * The events the elements emit, by name, each a `CustomEvent` with its detail: `MosaicaEventMap`
 * from `@mosaicajs/components`.
 */
export type MosaicaEvents = MosaicaEventMap;

const ev = <K extends keyof MosaicaEvents>(name: K) => name as EventName<MosaicaEvents[K]>;

const wrap = <E extends HTMLElement, Ev extends Record<string, EventName>>(tagName: string, elementClass: new () => E, events: Ev) =>
  createComponent({react: React, tagName, elementClass, events, displayName: elementClass.name});

/**
 * Hands the element one function per mount in place of the `authorise` prop, and calls the latest
 * prop through a ref, as `useMosaicaStore` does. An element builds a new store when its `authorise`
 * function changes, so an inline arrow passed straight through would build one on every render.
 * The store is built again when `authorise` is given or removed; to show another viewer, change
 * `token`, `viewerUrl` or the component's `key`.
 */
const withStableAuthorise = <C extends React.ForwardRefExoticComponent<any>>(Inner: C): C => {
  const Outer = React.forwardRef<unknown, {authorise?: TokenSupplier | null}>((props, ref) => {
    const latest = React.useRef(props.authorise);
    React.useLayoutEffect(() => {
      latest.current = props.authorise;
    });
    const given = props.authorise != null;
    const authorise = React.useMemo(() => (given ? () => latest.current!() : null), [given]);
    return React.createElement(Inner, {...props, authorise, ref});
  });
  Outer.displayName = Inner.displayName;
  return Outer as unknown as C;
};

/** The handler props for events a map emits, which the explorer also carries. */
const mapEvents = {
  onPick: ev('mosaica-pick'),
  onMiss: ev('mosaica-miss'),
  onHover: ev('mosaica-hover'),
  onViewChange: ev('mosaica-viewchange'),
  onSelectChange: ev('mosaica-selectchange'),
  onArtifactOpen: ev('mosaica-artifactopen'),
  onLayerChange: ev('mosaica-layerchange')
};

/** `<mosaica-store>` as a React component. It has no event props. */
export const MosaicaStore = withStableAuthorise(wrap('mosaica-store', StoreElement, {}));
/**
 * `<mosaica-explorer>` as a React component. It has a handler prop for every event, since the
 * elements inside it emit them and each event bubbles out of it: `onPick`, `onMiss`, `onHover`,
 * `onViewChange`, `onSelectChange`, `onArtifactOpen`, `onLayerChange`, `onArtifactFit`,
 * `onColourChange`, `onLevelChange`, `onValueColour`, `onClusterColour`, `onPaletteChange`, `onClusterPaletteChange`,
 * `onDisplayChange`, `onSizeChange`, `onBudgetChange`, `onClusterBudgetChange`, `onFold`,
 * `onFilterChange`, `onStateChange`, `onExpired`, `onOpen`, `onClose`, `onClauseChange`,
 * `onViewSwitch` and `onViewFollow`, each for the `mosaica-` event of the same name in lower case.
 */
export const MosaicaExplorer = withStableAuthorise(wrap('mosaica-explorer', ExplorerElement, {
  ...mapEvents,
  onArtifactFit: ev('mosaica-artifactfit'),
  onColourChange: ev('mosaica-colourchange'),
  onLevelChange: ev('mosaica-levelchange'),
  onValueColour: ev('mosaica-valuecolour'),
  onClusterColour: ev('mosaica-clustercolour'),
  onPaletteChange: ev('mosaica-palettechange'),
  onClusterPaletteChange: ev('mosaica-clusterpalettechange'),
  onDisplayChange: ev('mosaica-displaychange'),
  onSizeChange: ev('mosaica-sizechange'),
  onBudgetChange: ev('mosaica-budgetchange'),
  onClusterBudgetChange: ev('mosaica-clusterbudgetchange'),
  onFold: ev('mosaica-fold'),
  onFilterChange: ev('mosaica-filterchange'),
  onStateChange: ev('mosaica-statechange'),
  onExpired: ev('mosaica-expired'),
  onOpen: ev('mosaica-open'),
  onClose: ev('mosaica-close'),
  onClauseChange: ev('mosaica-clausechange'),
  onViewSwitch: ev('mosaica-viewswitch'),
  onViewFollow: ev('mosaica-viewfollow')
}));
/**
 * `<mosaica-map>` as a React component. Event props: `onPick` (`mosaica-pick`), `onMiss`
 * (`mosaica-miss`), `onHover` (`mosaica-hover`), `onViewChange` (`mosaica-viewchange`), `onSelectChange`
 * (`mosaica-selectchange`), `onArtifactOpen` (`mosaica-artifactopen`) and `onLayerChange`
 * (`mosaica-layerchange`).
 */
export const MosaicaMap = withStableAuthorise(wrap('mosaica-map', MapElement, mapEvents));
/** `<mosaica-status>` as a React component. Event props: `onStateChange` (`mosaica-statechange`) and `onExpired` (`mosaica-expired`). */
export const MosaicaStatus = wrap('mosaica-status', StatusElement, {onStateChange: ev('mosaica-statechange'), onExpired: ev('mosaica-expired')});
/** `<mosaica-count>` as a React component. It has no event props. */
export const MosaicaCount = wrap('mosaica-count', CountElement, {});
/**
 * `<mosaica-item-card>` as a React component. Event props: `onOpen` (`mosaica-open`), `onClose`
 * (`mosaica-close`) and `onViewFollow` (`mosaica-viewfollow`).
 */
export const MosaicaItemCard = wrap('mosaica-item-card', ItemCardElement, {onOpen: ev('mosaica-open'), onClose: ev('mosaica-close'), onViewFollow: ev('mosaica-viewfollow')});
/**
 * `<mosaica-filter-panel>` as a React component. Event props: `onFilterChange`
 * (`mosaica-filterchange`), `onChipOpen` (`mosaica-chipopen`), and from its cards
 * `onClauseChange` (`mosaica-clausechange`), `onColourChange` (`mosaica-colourchange`),
 * `onLevelChange` (`mosaica-levelchange`), `onValueColour` (`mosaica-valuecolour`),
 * `onClusterColour` (`mosaica-clustercolour`) and `onFold` (`mosaica-fold`).
 */
export const MosaicaFilterPanel = wrap('mosaica-filter-panel', FilterPanelElement, {
  onFilterChange: ev('mosaica-filterchange'),
  onChipOpen: ev('mosaica-chipopen'),
  onClauseChange: ev('mosaica-clausechange'),
  onColourChange: ev('mosaica-colourchange'),
  onLevelChange: ev('mosaica-levelchange'),
  onValueColour: ev('mosaica-valuecolour'),
  onClusterColour: ev('mosaica-clustercolour'),
  onFold: ev('mosaica-fold')
});
/** `<mosaica-selection>` as a React component. Event prop: `onSelectChange` (`mosaica-selectchange`). */
export const MosaicaSelection = wrap('mosaica-selection', SelectionElement, {onSelectChange: ev('mosaica-selectchange')});
/** `<mosaica-layer-picker>` as a React component. Event prop: `onLayerChange` (`mosaica-layerchange`). */
export const MosaicaLayerPicker = wrap('mosaica-layer-picker', LayerPickerElement, {onLayerChange: ev('mosaica-layerchange')});
/** `<mosaica-view-picker>` as a React component. Event prop: `onViewSwitch` (`mosaica-viewswitch`). */
export const MosaicaViewPicker = wrap('mosaica-view-picker', ViewPickerElement, {onViewSwitch: ev('mosaica-viewswitch')});
/** `<mosaica-key-picker>` as a React component. Event prop: `onViewSwitch` (`mosaica-viewswitch`). */
export const MosaicaKeyPicker = wrap('mosaica-key-picker', KeyPickerElement, {onViewSwitch: ev('mosaica-viewswitch')});
/**
 * `<mosaica-artifact-card>` as a React component. Event props: `onArtifactFit`
 * (`mosaica-artifactfit`), `onClauseChange` (`mosaica-clausechange`) and `onClose`
 * (`mosaica-close`).
 */
export const MosaicaArtifactCard = wrap('mosaica-artifact-card', ArtifactCardElement, {
  onArtifactFit: ev('mosaica-artifactfit'),
  onClauseChange: ev('mosaica-clausechange'),
  onClose: ev('mosaica-close')
});
/**
 * `<mosaica-field-card>` as a React component. Event props: `onFilterChange`
 * (`mosaica-filterchange`), `onClauseChange` (`mosaica-clausechange`), `onColourChange`
 * (`mosaica-colourchange`), `onLevelChange` (`mosaica-levelchange`), `onValueColour`
 * (`mosaica-valuecolour`), `onClusterColour` (`mosaica-clustercolour`) and `onFold` (`mosaica-fold`).
 */
export const MosaicaFieldCard = wrap('mosaica-field-card', FieldCardElement, {
  onFold: ev('mosaica-fold'),
  onFilterChange: ev('mosaica-filterchange'),
  onClauseChange: ev('mosaica-clausechange'),
  onColourChange: ev('mosaica-colourchange'),
  onLevelChange: ev('mosaica-levelchange'),
  onValueColour: ev('mosaica-valuecolour'),
  onClusterColour: ev('mosaica-clustercolour')
});
/**
 * `<mosaica-colour-editor>` as a React component. Event props: `onValueColour`
 * (`mosaica-valuecolour`) and `onClusterColour` (`mosaica-clustercolour`).
 */
export const MosaicaColourEditor = wrap('mosaica-colour-editor', ColourEditorElement, {
  onValueColour: ev('mosaica-valuecolour'),
  onClusterColour: ev('mosaica-clustercolour')
});
/**
 * `<mosaica-hierarchy>` as a React component. Event props: `onClauseChange` (`mosaica-clausechange`)
 * and `onArtifactFit` (`mosaica-artifactfit`).
 */
export const MosaicaHierarchy = wrap('mosaica-hierarchy', HierarchyElement, {onClauseChange: ev('mosaica-clausechange'), onArtifactFit: ev('mosaica-artifactfit')});

export type {
  ArtifactCardElement,
  ColourEditorElement,
  CountElement,
  ExplorerElement,
  FieldCardElement,
  FilterPanelElement,
  HierarchyElement,
  ItemCardElement,
  KeyPickerElement,
  LayerPickerElement,
  MapElement,
  SelectionElement,
  StatusElement,
  StoreElement,
  ViewPickerElement
};
