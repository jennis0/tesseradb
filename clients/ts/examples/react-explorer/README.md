# The React explorer

Vite + React 19. `<TesseraExplorer>` from `@tesseradb/react/components` over a store the host
owns (`useTesseraStore`), with the `detail` slot replaced by a host component reading
`useProjection(store, 'selection')` — `src/ItemCard.tsx`.

```bash
node ../plain-html/server.mjs &      # the app server: tokens, and the users it knows
npm run dev                          # http://localhost:5181
```

The store is keyed by the signed-in user (`<Session key={user}>`): a different user unmounts the
component, which disposes its store in the effect's cleanup, and mounts a fresh one. `StrictMode`
is on, because its double mount is what the hook is built for.
