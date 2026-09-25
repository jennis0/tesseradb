# The layer in a host's Deck

`TesseraLayer` from `@tesseradb/deck` in a `Deck` the page builds itself, with no element. The page has an `OrthographicView` with `flipY: true` and its own camera (`src/view.ts`). `viewInputOf` turns each camera change into a `store.setView` call. A `PolygonLayer` of the host's own draws the edge of the world square in the same 512-unit coordinates as the marks. The layer is given the store and nothing else. It makes its GPU buffers and lookup texture on the deck's device and releases them when it leaves the layer list.

It expects the demo's `2m4` scale (`./run_demo.sh --scale 2m4` at the repository root), whose users `../plain-html/users.json` lists.

```bash
node ../plain-html/server.mjs &      # the app server, which mints the tokens
npm run dev                          # http://localhost:5183
```

Not built yet: a coordinate adapter for a geographic `MapView` or a MapLibre host. The layer draws in the orthographic world square only.
