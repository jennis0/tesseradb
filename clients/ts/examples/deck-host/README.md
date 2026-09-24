# The layer in a host's Deck

`TesseraLayer` from `@tesseradb/deck` in a `Deck` the page builds itself, with no element: an
`OrthographicView` with `flipY: true`, the host's own camera (`src/view.ts`), `viewInputOf`
turning each camera change into `store.setView`, and a `PolygonLayer` of the host's own drawn
beside the marks in the same 512-unit world coordinates. The layer is given the store and nothing
else: it makes its GPU buffers and lookup texture on the deck's device and releases them when it
leaves the layer list.

```bash
node ../plain-html/server.mjs &      # the app server: tokens, and the users it knows
npm run dev                          # http://localhost:5183
```

Not built yet: a coordinate adapter for a geographic `MapView` or a MapLibre host. The layer draws
in the orthographic world square only.
