<!-- The pages under components/ are generated from the element sources in clients/ts/components. Edit the doc comments there, then run: node clients/ts/scripts/reference.mjs -->

# Components

The `tessera-*` custom elements, in the `@tesseradb/components` package, built with Lit. Each element's page lists its attributes, properties, methods, events, slots, parts and CSS custom properties, generated from its source.

| Element | What it is |
|---|---|
| [`<tessera-explorer>`](components/tessera-explorer.md) | The map and every panel, in a default layout. |
| [`<tessera-map>`](components/tessera-map.md) | The map canvas. |
| [`<tessera-store>`](components/tessera-store.md) | Provides one store to the elements inside it. |
| [`<tessera-status>`](components/tessera-status.md) | The state and the counts of the view, on one line. |
| [`<tessera-count>`](components/tessera-count.md) | A sample count or a masked count, formatted. |
| [`<tessera-legend>`](components/tessera-legend.md) | What the map's colours mean, and the colour controls. |
| [`<tessera-layer-picker>`](components/tessera-layer-picker.md) | Which annotation layers the map draws. |
| [`<tessera-view-picker>`](components/tessera-view-picker.md) | Chooses the view. |
| [`<tessera-key-picker>`](components/tessera-key-picker.md) | Chooses the view within the current group. |
| [`<tessera-filter-panel>`](components/tessera-filter-panel.md) | Every filter control, with the applied clauses as chips. |
| [`<tessera-filter>`](components/tessera-filter.md) | One filter control, drawn by the column's type. |
| [`<tessera-selection>`](components/tessera-selection.md) | The selected region's counts, marks and actions. |
| [`<tessera-item-card>`](components/tessera-item-card.md) | The selected item's fields, with Open and Copy id. |
| [`<tessera-artifact-list>`](components/tessera-artifact-list.md) | The artifacts served for the view, with their counts. |
| [`<tessera-artifact-card>`](components/tessera-artifact-card.md) | The opened artifact, with its lineage and its filter buttons. |
| [`<tessera-hierarchy>`](components/tessera-hierarchy.md) | A layer's hierarchy, browsed apart from the viewport. |

[Events](components/events.md) lists every event with its `detail`, and [Theme tokens](components/tokens.md) every CSS custom property with its defaults.

## Loading the elements

`import '@tesseradb/components'` defines every element. Each element also has its own entry, such as `import '@tesseradb/components/map'`, which defines that element and the elements it renders. The map, the explorer, the legend, the artifact list and the artifact card import `@tesseradb/deck`, which depends on deck.gl; the other elements do not. `@deck.gl/aggregation-layers` is a peer dependency as well, which `@tesseradb/deck` imports only when a map first draws density as hexagons or contours. A host's bundler puts it in a chunk of its own, which a page that never draws those does not load.

A page with no build step loads the single-file bundle, `tessera-components.js`, which holds Lit, deck.gl, its aggregation layers and the decode worker. `npm run bundle -w @tesseradb/components` in `clients/ts` writes it to `components/dist/` with its subresource-integrity hash in `tessera-components.js.sri`:

```html
<script type="module" src="./tessera-components.js" integrity="sha384-..."></script>
```

A tag already defined keeps its first definition, so two copies of the package on one page do not clash.

## How an element finds its store

Every element but `<tessera-count>` reads a store, and takes the first of these that it has:

1. its `store` property;
2. the store of the nearest `<tessera-store>` or `<tessera-explorer>` above it;
3. for `<tessera-map>`, `<tessera-explorer>` and `<tessera-store>` only, a store it builds from its `viewer-url` attribute and its `token` attribute or `authorise` property.

With none, the element renders its detached state. An element that has built its own store keeps it when a provider appears above it later. A store the element built is replaced when `viewer-url`, `token` or the `authorise` function changes, and disposed by the element's `dispose()`. A store serves one viewer, as `Store` in the [TypeScript reference](typescript.md) sets out: to show another viewer, give the element a new `token`, a new `authorise` function, a new `viewer-url`, or a new element. Without one of these, the previous viewer's data stays on screen for as long as `Store` in the TypeScript reference says. An element also drops the pages, names and hover it fetched itself when it adopts another store and when its store forgets what the server answered. A store it was given is left for its owner to dispose. Disconnecting an element keeps its store, so moving it in the page does not fetch the view again.

## Events and styling

Every event is a `CustomEvent` that bubbles and is composed, so a host listens on the element or on any ancestor, including one outside `<tessera-explorer>`.

An element's parts are styled with `::part()`. An element that renders another inside its shadow root forwards the inner element's parts under its name: through `<tessera-explorer>`, the map's toolbar is `::part(map-controls)` and the item card's title is `::part(item-card-title)`. `PARTS` in `@tesseradb/components` lists each element's parts, for a host that renders an element in its own shadow root and forwards them.
