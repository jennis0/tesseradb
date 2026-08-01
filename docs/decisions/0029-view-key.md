# 0029 — A fourth concept called "epoch": the view key

**Date:** 2026-08-01 · **Status:** Settled · **Extends:** [0026](0026-idset-stamp-version.md)

## Context

Applying decision 0026 turned up a fourth concept that had also been called "epoch", in around
seventy places across the client-facing documents and the TypeScript client. It is none of the
three that decision covered.

It is the composite **(mask, overlay version, slice, k, idset)** — the coordinate within which
`served(viewport)` is stable. A client keys its cache on it and flips atomically between its
values; the server guarantees stability within one.

Four concepts reached for the same word because three of the four are versioning-shaped and this
one is composed partly of the others, so "epoch" fitted all of them equally badly.

## Decision

It is the **view key**.

**The viewport is not part of it.** That is the point of the concept: a served viewport is stable
*across* viewports within a single view key.

"Key" rather than "state" for that reason specifically. A key identifies; a state describes. The
candidate "view state" invited a reader to assume the camera was included — where the map is
looking, what is on screen — which is exactly what the concept excludes, and it would have needed a
clarifying sentence in every document that used it. A name that requires a disclaimer is the wrong
name.

It also matches use: a client holds one cache entry per view key, and flips atomically between
them.

## Why it matters more than the other three

It is the only one of the four a **client author** must understand. They key a cache on it, and a
cache keyed too loosely serves one principal's authorised data to another — a disclosure rather
than a staleness bug.

## A correction to 0026

0026's third rename was unnecessary and is not being made. The slices design had already replaced
its own use of "epoch" with `flush` and `base`, which carry more information than `version` would;
renaming it would have been rescoping rather than disambiguating. The ambiguity 0026 exists to
remove was already gone there.

## Evidence

`docs/design/client-interaction.md` §6, §7.2 and the stability statement at §9;
`docs/design/caching.md`; `docs/design/tile-addressed-integration.md`; `clients/ts/core/src/`.
