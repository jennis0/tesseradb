# The plain-HTML example

One page with no build step. `index.html` loads the single-file bundle by relative path with its integrity hash, and mounts `<tessera-explorer>` with a `viewer-url` and a `token`. `server.mjs` is the app server beside it: it serves the page and the bundle, mints tokens, and proxies the viewer routes.

```bash
npm run bundle -w @tesseradb/components    # writes components/dist/tessera-components.js and its .sri
node server.mjs                            # http://localhost:5180
```

`users.json` lists the signed-in users the page offers and the access labels each holds. The ones shipped match the demo's presets for the `2m4` bundle. `TESSERA_VIEWER_URL`, `TESSERA_SESSION_URL` and `TESSERA_SESSION_CRED` point the server at another deployment, and `PORT` moves it off 5180.

## Where the token comes from

The page never holds the deployment's session credential. `server.mjs` does. When the page asks `POST /token` for its user, the server calls `POST /session/authorise` with that user's labels and returns the viewer token (`authorise()` in `server.mjs`). A real application takes the user from its own sign-in instead of a query parameter.

Under the `builtin:passthrough` auth plugin the server takes the labels it is sent as given. This app server therefore decides what each of its users may see, and the answer is only as trustworthy as the sign-in behind it.

## Reaching the viewer routes

The page calls the viewer routes on its own origin. `server.mjs` proxies `/v1/*` to the viewer listener and forwards every response header. The client reads `etag`, `x-tessera-identity-key`, `x-tessera-pin`, `x-tessera-stale`, `x-tessera-region`, `x-tessera-server-us` and `x-tessera-admission-us`, so a proxy in front of the viewer listener has to pass them through.

A deployment can instead list the page's origin in `serve.cors_origins`. That admits the page to the viewer listener only, and the session listener stays closed to browsers. `serve.dev_cors_origins`, which the demo uses, also opens the session listener and is for development only.

## The other examples

`../react-explorer`, `../canvas-store` and `../deck-host` get their tokens from this server: their Vite dev servers proxy `/token` and `/users` here and `/v1/*` to the viewer listener. Start this one first.
