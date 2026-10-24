# The React explorer

Vite and React 19. `<MosaicaExplorer>` from `@mosaicajs/react/components` draws a store the host owns (`useMosaicaStore`). Its `detail` slot holds a host component, `src/ItemCard.tsx`, which reads `useProjection(store, 'selection')`.

It expects the demo's `2m4` scale (`./run_demo.sh --scale 2m4` at the repository root), whose users `../plain-html/users.json` lists. If `clients/ts/node_modules` is absent, run `npm ci` in `clients/ts` first.

Run these in this directory. The app server refuses to start without an API key holding `authorise-as` in `MOSAICA_API_KEY`. `run_demo.sh` writes one to `mosaica-demo/presets/2m4.key` at the repository root.

```bash
MOSAICA_API_KEY=$(cat ../../../../mosaica-demo/presets/2m4.key) node ../plain-html/server.mjs &   # the app server, which mints the tokens
npm run dev                          # http://localhost:5181
```

The dev server proxies `/token` and `/users` to the app server at `http://127.0.0.1:5180`, or at `MOSAICA_APP_URL` where that is set, and `/v1/*` to the viewer listener at `http://127.0.0.1:37585`, or at `MOSAICA_VIEWER_URL`.

The store is keyed by the signed-in user (`<Session key={user}>`). Choosing another user unmounts the component, which disposes its store in the effect's cleanup, and mounts a new one. `StrictMode` is on, so every run exercises the double mount the hook has to survive.
