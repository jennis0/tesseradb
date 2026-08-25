# 0095 — One Python package, `tesseradb`, and the `@tesseradb` npm scope

**Date:** 2026-08-24 · **Status:** Settled (owner ruling, in conversation on the client-components
design; transcribed 2026-08-25 from [`client-components.md`](../design/client-components.md) §11 and
the handover's §1).

## Context

The client-components design (D6) needed a home for the notebook widget, and later the Python SDK
([#47]) and an in-process Tessera. The repository's two npm packages were `@tessera/client` and
`@tessera/viewer`; `tessera` is taken on npm.

## The decision

- **One Python package, `tesseradb`.** The widget ships as its `[widget]` extra (anywidget plus the
  bundle, built by the wheel's build hook at wheel-build time — a user installing from PyPI needs
  no Node). The SDK and the in-process instance join the same package later. It shares no code with
  `reference/`.
- **The npm scope is `@tesseradb/*`**, to match. The repository's `@tessera/client` and
  `@tessera/viewer` take the new scope at the design's step 1 — no deployment holds the old names
  ([0048](0048-no-deployments-exist-so-delete-rather-than-support.md)).
- Client-architecture's D4 (the viewer keeps its *name*) stands; what the viewer *holds* changes,
  so D4 is amended in scope, not kept.

## Why

The product is Tessera; the scope is `@tesseradb` only because `tessera` was taken, and a custom
element's tag has no registry, so tags stay `tessera-*`. One Python package because a user should
`pip install` one thing whether they want the widget, the SDK or both.
