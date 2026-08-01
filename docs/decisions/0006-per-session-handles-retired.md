# 0006 — Per-session `u32` handles are retired from the viewer plane

**Date:** 2026-07-29 · **Status:** Settled

## Decision

The viewer plane no longer issues per-session `u32` handles. It carries `tessera_id` directly.
The handle mechanism is retained only for Phase 3 **node** handles, which remain per-session-keyed.

## Why

With [0005](0005-tessera-id-keyed-bijection.md) the identifier is already opaque and stable, so a
second per-session indirection bought nothing and cost a table on every session.

## The constraint that survives the mechanism

Whatever Phase 3 node handles become, the decoded worker-local reference must be an index into the
worker's own handle table and **never an entity ID**. Putting an entity ID in a permutation's
plaintext would ship corpus identifiers to the router.

## Evidence

Design r21; contracts §0.3 deviation 8. `crates/tessera-wire/tests/wire.rs` names the boundary
change. `crates/tessera-wire/src/handles.rs` survives `#[allow(dead_code)]` and records that the
keyed-permutation half is deferred.
