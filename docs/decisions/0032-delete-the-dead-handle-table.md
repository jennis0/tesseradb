# 0032 — The per-session handle allocation goes; the type stays

**Date:** 2026-08-01 · **Status:** Settled · **Follows:** [0006](0006-per-session-handles-retired.md)

## Context

Per-session handles were retired from the viewer plane: the wire carries an opaque identifier
directly. The handle machinery was deliberately kept for Phase 3 node handles, where the identity
genuinely is per-session — a frontier node is a query-time object rather than a corpus object.

But every session still allocates a handle table, and **nothing reads or writes it**.

## Decision

**Remove the allocation from the per-session entry. Keep the type.**

`HandleTable` stays in the wire crate with its dead-code marker and its note about Phase 3, because
that type is where the constraint node handles must obey is recorded: a decoded worker-local
reference is an index into the worker's own table and **never an entity ID**, since putting an
entity ID into a permutation's plaintext would ship corpus identifiers to the router.

## Why

Rebuilding a per-session allocation when there is finally something to put in it costs nothing.
Carrying a dead one costs an allocation per session and, more importantly, gives a reader a
structure to reason about that does nothing — the repo's convention is that code records settled
decisions rather than intentions.

## Evidence

Register row S24. `crates/tessera-server/src/state.rs`; `crates/tessera-wire/src/handles.rs`.
