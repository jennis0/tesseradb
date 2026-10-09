# The store on a canvas

The store with none of Mosaica's rendering. `src/main.ts` calls `createStore`, calls `setView` from a hand-written camera (`src/camera.ts`) whenever it moves, and draws the `marks` projection as dots on a 2D canvas. The host formats the counts itself with `formatCount` and `formatMasked`. The only Mosaica dependency is `@mosaica/client`: no Lit, no deck.gl and no elements.

It expects the demo's `2m4` scale (`./run_demo.sh --scale 2m4` at the repository root), whose users `../plain-html/users.json` lists.

```bash
node ../plain-html/server.mjs &      # the app server, which mints the tokens
npm run dev                          # http://localhost:5182
```
