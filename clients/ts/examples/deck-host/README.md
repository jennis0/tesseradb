# The layer in a host's Deck

`MosaicaLayer` from `@mosaicajs/deck` in a `Deck` the page builds itself, with no element. The page has an `OrthographicView` with `flipY: true` and its own camera (`src/view.ts`). `viewInputOf` turns each camera change into a `store.setView` call. A `PolygonLayer` of the host's own draws the edge of the world square in the same 512-unit coordinates as the marks. The layer is given the store and nothing else. It makes its GPU buffers and lookup texture on the deck's device and releases them when it leaves the layer list.

It expects the demo's `2m4` scale (`./run_demo.sh --scale 2m4` at the repository root), whose users `../plain-html/users.json` lists. If `clients/ts/node_modules` is absent, run `npm ci` in `clients/ts` first.

Run these in this directory. The app server refuses to start without an API key holding `authorise-as` in `MOSAICA_API_KEY`. `run_demo.sh` writes one to `mosaica-demo/presets/2m4.key` at the repository root.

```bash
MOSAICA_API_KEY=$(cat ../../../../mosaica-demo/presets/2m4.key) node ../plain-html/server.mjs &   # the app server, which mints the tokens
npm run dev                          # http://localhost:5183
```

The dev server proxies `/token` and `/users` to the app server at `http://127.0.0.1:5180`, or at `MOSAICA_APP_URL` where that is set, and `/v1/*` to the viewer listener at `http://127.0.0.1:37585`, or at `MOSAICA_VIEWER_URL`.

Not built yet: a coordinate adapter for a geographic `MapView` or a MapLibre host. The layer draws in the orthographic world square only.
