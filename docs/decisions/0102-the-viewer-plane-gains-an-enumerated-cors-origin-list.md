# 0102 — The viewer plane gains an enumerated production CORS origin list; the session plane does not

**Date:** 2026-08-26 · **Status:** Settled (owner ruling — D10 of
[`client-components.md`](../design/client-components.md) §3, ruled option (a); ⊘ the server change
is not built, tracked as S1 in [`client-delivery.md`](../client-delivery.md)).

## Context

A drop-in `<tessera-explorer>` on a customer's page is a browser holding a **token** and calling
the viewer plane — client-interaction §7's T2. From any origin but Tessera's own that needs CORS,
and the only CORS the server has is `serve.dev_cors_origins`: off unless typed, no wildcard, no
environment variable, and logged as a development seam at startup. Without a production
equivalent, a drop-in works only behind a same-origin reverse proxy of `/v1/*` forwarding the six
headers a replica is keyed and revalidated by, and the notebook widget's browser-direct arm does
not exist at all.

## The decision

**`serve.cors_origins`, enumerated, on the viewer plane only.**

- Enumerated origins; no wildcard, and none by default.
- **The session plane stays closed to browsers.** `POST /session/authorise` is gated by the
  deployment's session credential, which a browser must never hold; only the token reaches one.
- The widget ships its **browser-direct** arm on this surface (D4, ruled with it). The proxy arm —
  a small Jupyter server extension holding the credential — stays **documented and not built**, for
  the containers an origin list cannot name: a VS Code notebook renders at `vscode-webview://` and
  Colab in a sandboxed iframe, neither of which can be enumerated.

## Why

This is a **token-presentation** surface. The token is already per-principal, already scoped by
what the server decided that principal may see, and already expires; letting a named origin present
one creates no authority that did not exist. The earlier "no" was argued against exposing the
*credential* to a browser, which this does not do — that is exactly why the session plane is
excluded rather than the pair being decided together.

The alternative is not "more secure", it is a reverse proxy in front of every deployment that wants
a drop-in, whose own correctness — forwarding six headers unmangled — is a thing that can be got
wrong quietly, in the integrator's infrastructure, where nobody here can see it.

## Consequences

- The startup warning stays for `dev_cors_origins`, which remains a development seam and is not
  what this replaces.
- An origin list is a deployment's statement about which pages may present its tokens. It is not an
  authorisation boundary, and nothing in the engine may come to treat it as one.
- Client-interaction §15's open item — tokens are bound to their issuing session — is settled by
  the same ruling: a token presented from an enumerated origin is the session it was minted in.
