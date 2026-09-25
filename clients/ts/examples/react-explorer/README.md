# The React explorer

Vite and React 19. `<TesseraExplorer>` from `@tesseradb/react/components` draws a store the host owns (`useTesseraStore`). Its `detail` slot holds a host component, `src/ItemCard.tsx`, which reads `useProjection(store, 'selection')`.

```bash
node ../plain-html/server.mjs &      # the app server, which mints the tokens
npm run dev                          # http://localhost:5181
```

The store is keyed by the signed-in user (`<Session key={user}>`). Choosing another user unmounts the component, which disposes its store in the effect's cleanup, and mounts a new one. `StrictMode` is on, so every run exercises the double mount the hook has to survive.
