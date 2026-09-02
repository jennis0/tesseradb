# 0123 — Cross-column suggestion is deferred

**Date:** 2026-09-02 · **Status:** Settled (owner ruling)

## What this answers

Whether a single search box over every category column — `GET /v1/suggest?q=`, one response naming
which column each value came from — ships with the per-column verb.
`value-suggestion.md` §5.4 put it; this is ruling F of its §10.

## The decision

**Deferred.** Only `GET /v1/categories/{column}/suggest` exists. A client wanting one box over
several columns issues one request per column and merges the results.

## Why

It is composable from the per-column verb and adds nothing but round trips saved. Each column is
gated on its own member sets, so a server-side form runs the same per-column predicate the client
would have asked for separately — there is no shared work to recover and no gate to state once
instead of many times. It earns its place when a client actually wants it.

## What this does not change

Its shape is settled if it is ever built: the per-column response, with `column` carried per value
rather than once at the top. Nothing in the per-column verb is designed around the possibility.
