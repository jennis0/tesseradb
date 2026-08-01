# 0024 — The leak register covers what a viewer can infer, not data at rest

**Date:** 2026-08-01 · **Status:** Settled

## Context

Authorisation results are persisted: a viewer's visible set is written to an engine-local cache
directory — never into the bundle — so that a repeated grant set is reused across process restarts
instead of re-unioning postings.

It is a durable artifact, outside the bundle, holding a materialised answer to "who can see what",
and it appeared in neither the leak register nor the cache table.

## Decision

**It goes in the cache table with its key, and its integrity argument goes into the specification.
It does not get a leak-register row.**

The register's scope is **what a viewer can infer**. Every existing row is of that kind: cluster
membership inferred from density, timing inferred from response latency, identifier structure
inferred from gaps, correlation inferred across sessions. The persisted fragment is not
viewer-facing at all — its threat is an attacker with filesystem access, which is a different
reader, a different mitigation and a different audience.

Mixing the two would make the register a list of everywhere data lives rather than a list of what
a viewer can learn, and the second is what makes it possible to be exhaustive.

## What the specification must carry

The integrity argument, because a parseable-but-wrong fragment would be a silent disclosure rather
than a crash. The cache therefore does not rest on the directory being engine-private:

- entries are content-addressed with a stored digest, verified on **every** reopen and **before**
  the unsafe frozen view is constructed;
- writes are fsynced before the rename that makes them visible;
- the directory and its files are created owner-only.

## Evidence

Register row A3. `crates/tessera-authz/src/fragment.rs`; architecture §8.5's cache table.
