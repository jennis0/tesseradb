# The store on a canvas

C2 with none of Tessera's rendering: `createStore`, `setView` from a hand-rolled camera
(`src/camera.ts`) on a 2D canvas, `subscribe('marks')` drawing dots, and the counts formatted by
the host with `formatCount` and `formatMasked` — the type says which. The package's dependencies
are `@tesseradb/client` alone: no Lit, no deck.gl, no element.

```bash
node ../plain-html/server.mjs &      # the app server: tokens, and the users it knows
npm run dev                          # http://localhost:5182
```
