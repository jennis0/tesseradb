# The plain-HTML example

One page with no build step. `index.html` loads the single-file bundle by relative path with its integrity hash, and mounts `<tessera-explorer>` with a `viewer-url` and a `token`. `server.mjs` is the app server beside it: it serves the page and the bundle, mints tokens, and proxies the viewer routes.

```bash
npm run bundle -w @tesseradb/components    # writes components/dist/tessera-components.js and its .sri
node server.mjs                            # http://localhost:5180
```

The server expects the demo's `2m4` scale (`./run_demo.sh --scale 2m4` at the repository root): a viewer listener on `127.0.0.1:37585` and a session listener on `127.0.0.1:49303`. `TESSERA_API_KEY` is an API key whose principal holds `authorise-as`; `run_demo.sh` writes one to `tessera-demo/presets/2m4.key` at the repository root. `users.json` lists the signed-in users the page offers and the Tessera principal each reads as. Those principals are the ones `run_demo.sh` creates when it measures the 2m4 presets, named `holding-` and a digest of their terms; `tessera principal list` shows them. `TESSERA_VIEWER_URL` and `TESSERA_SESSION_URL` point the server at another deployment, whose principals `users.json` then has to name, and `PORT` moves it off 5180. Until the bundle is built the server answers the page with a 503 naming the command above.

## Where the token comes from

The page never holds the API key. `server.mjs` does. When the page asks `POST /token` for its user, the server calls `POST /session/authorise` naming that user's principal and returns the viewer token (`authorise()` in `server.mjs`). A real application takes the user from its own sign-in instead of a query parameter.

The key can mint a session for any principal holding `read`. This app server therefore decides which principal each of its users reads as, and the answer is only as trustworthy as the sign-in behind it. What each principal may see is decided by the terms the deployment's catalogue grants it.

## Reaching the viewer routes

The page calls the viewer routes on its own origin. `server.mjs` proxies `/v1/*` to the viewer listener and forwards every response header. The client reads `etag`, `x-tessera-identity-key`, `x-tessera-pin`, `x-tessera-stale`, `x-tessera-region`, `x-tessera-server-us` and `x-tessera-admission-us`, so a proxy in front of the viewer listener has to pass them through.

A deployment can instead list the page's origin in `serve.cors_origins`. That admits the page to the viewer listener only, and the session listener stays closed to browsers. `serve.dev_cors_origins`, which the demo uses, also opens the session listener and is for development only.

## The other examples

`../react-explorer`, `../canvas-store` and `../deck-host` get their tokens from this server: their Vite dev servers proxy `/token` and `/users` here and `/v1/*` to the viewer listener. Start this one first.
