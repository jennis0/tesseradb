# 0035 — The session registry sheds on growth, not on a timer

**Date:** 2026-08-01 · **Status:** Settled

## Context

The per-token session registry refused expired sessions and never removed them, so both of its maps
grew for the life of the process at one entry per `/session/authorise` call. The cost was not the
map entry: each retained session holds an `Arc<FrozenFragment>` — a live mask mapping — so the
fragment cache's byte bound could not release what a dead session referenced. Dead sessions pinned
exactly the memory that bound exists to release.

Something had to sweep. The question was what triggers it, because the registry's mutex is taken by
every viewer request and a sweep is an O(n) pass under it.

## Decision

**The sweep runs on insert, throttled, and there is no timer.** After each pass the next is due at
`max(2 × live, 16)` retained entries. That gives amortised O(1) per authorisation, retention bounded
at twice the live set, and a worst-case pause of O(retained) — published on `/control/status` as
`sessions.retained`, beside `sweeps`, `swept_total` and `sweep_at`.

**Sweeping is a memory mechanism and never an authorisation one.** An expired session is refused by
the deadline check whether or not a sweep has run, and a revocation removes its entry in its own
handler. A sweep only ever removes; nothing about it can delay, defer or undo a revocation.

## Why not a timer

A periodic task has to be spawned by whoever builds the runtime. `tessera serve` does; embedders,
`mount_server` and every integration test do not — so the property would hold in one deployment
shape and silently not in the others. That is the same objection the deny lane's runtime records
against arithmetic that is only sufficient where this crate builds the runtime. Growth-triggered,
the sweep is a property of the registry type: no runtime, deterministic, and observable in a test
that never waits on a clock.

The trigger also sits on the event it is chasing. Authorisation is the only path that grows the
registry and the only path that builds new fragments, so pressure and sweeping arrive together.

## Cost, stated

After the last authorisation, up to `2 × live + 16` expired entries remain until the next one. That
residue is bounded and is not growing, and in that regime nothing is competing for the memory it
holds. A quiescent process does not reclaim it.

Two smaller consequences, recorded so they are not read as defects. Removing an expired session at
the point of refusal — O(1), and tempting — is deliberately not done: it would make the
`expired-token` 403 unobservable on any retry, a diagnostic loss on the path that tells a client to
re-authorise, for memory the sweep already reclaims. And once a session *is* swept it becomes
indistinguishable from one that never existed, so the 401/403 split is best-effort diagnostics and
no client may read it as a statement about whether a token was ever valid.

## Evidence

`crates/tessera-server/src/state.rs` carries the argument at the type.
`crates/tessera-server/tests/http_engine_state.rs` has four cases: retention is bounded, live
sessions survive, revocation takes effect with no sweep having run, and an expired session is
refused while still retained. Each was verified by planting the corresponding failure.
