# The plain-HTML example

One page, no build step: the self-contained bundle by relative path with its integrity hash, and
`<tessera-explorer>` with a `viewer-url` and a `token`. Beside it, `server.mjs` — the app server
every C1 host has, in its smallest form.

```bash
npm run build -w @tesseradb/components      # the bundle, dist/tessera-components.js and its .sri
node server.mjs                             # http://localhost:5180
```

`users.json` names the signed-in users the page offers and the claims each carries; the ones
shipped are the demo's five presets on the 2m4 bundle. `TESSERA_SESSION_URL`,
`TESSERA_VIEWER_URL` and `TESSERA_SESSION_CRED` point the server at another deployment.

## Where the token comes from

The page never holds the deployment's *session credential*. `server.mjs` does, and when the page
asks `POST /token` for its user, the server calls `POST /session/authorise` with that user's
claims and hands back the viewer token — `authorise()` in `server.mjs`, ten lines. A real
application takes the user from its own sign-in rather than from a query parameter; nothing
else changes.

What authority that call carries is the auth plugin's business, not the client's. Today the only
plugin is `builtin:passthrough`, which takes bare claims: the server behind this page asserts
what its user may see, and Tessera believes it. **Under `builtin:passthrough` this server is the
claim-minting proxy** — the shape client-interaction §7 documents as the anti-pattern — by
construction, until a verified-assertion plugin exists (design client-components D14, server-side,
asked for). A working example that is quietly that shape is how the anti-pattern ships, so this
one says what it is: the claims it mints are exactly as trustworthy as the sign-in behind it.

## In production

The page reaches the viewer plane on its **own origin**: `server.mjs` proxies `/v1/*` to the
viewer plane and forwards every response header, which is what keeps the six a replica is keyed
by — `etag`, `x-tessera-identity-key`, `x-tessera-pin`, `x-tessera-stale`,
`x-tessera-server-us`, `x-tessera-admission-us` (client-obligations rule 10). A proxy that drops
headers it does not know keeps none of them, and the client then cannot partition its cache by
principal or declare what it holds. A same-origin proxy of `/v1/*` forwarding those six is the
production topology today; the demo's `serve.dev_cors_origins` is a development seam and is not
one.

⊘ **Asked for, not ruled (D10).** A viewer-plane `serve.cors_origins` — enumerated, viewer plane
only, the session plane never browser-facing — would let the page present its token to the
viewer plane directly and replace the proxy. Until it is granted, the proxy is the only
production shape, and this example is that shape.

## The other two examples

`../react-explorer` and `../canvas-store` use this server for their tokens: their Vite dev servers
proxy `/token` and `/users` here and `/v1/*` to the viewer plane, so start this one first.
