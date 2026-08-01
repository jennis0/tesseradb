# 0036 — The buffered-body window gets a per-connection ceiling, not a connection cap

**Date:** 2026-08-01 · **Status:** Settled

## Context

`/control/ingest` buffers up to `ingest_max_batch_bytes` before its handler runs, because the whole
Arrow batch is decoded in one piece. The router's credential layer runs outside the extractors, so
an unauthenticated caller is refused with the body still an unconsumed stream — that half is closed.
What remained is a credentialed caller: each in-flight request pins a batch cap, and `axum::serve`
applies no connection cap, so the count is unbounded.

The existing startup relations weigh the *admitted* window — queued commands plus admitted handlers.
Requests that have not been admitted are in front of that window and are counted by nothing.

## Decision

**Bound the per-connection factor; document the count.**

`ingest.ingest_max_batch_bytes` is refused at startup above 64 MiB, four times the shipped default.
The count of connections is bounded by deployment posture: the control plane defaults to a unix
socket reachable only by admin systems, and where any plane is exposed more widely, a reverse proxy
is the connection bound. System architecture §8 states that as a deployment requirement.

## Why the ceiling is worth having on its own

Without it, `ingest_admission = 1, ingest_queue_bound = 1, ingest_max_batch_bytes = 8 GiB` satisfies
every existing relation — the admitted window is exactly at the resident ceiling — and then dies on
the *second* concurrent upload, before any admission bound has anything to say. A configuration
whose per-connection cost is unbounded makes an unbounded count catastrophic rather than merely
unbounded. The ceiling makes the multiplier the operator's, not the attacker's.

Its side effect is worth knowing: with a per-connection ceiling in place, the resident relation is
reachable only through the two count knobs. No legal batch cap can trip it against a shallow queue.

## Why neither in-process connection bound was taken

Both convert a prompt refusal into a wait, which is the property the ingest admission bound exists
to provide.

- A **`tower` concurrency limit** queues rather than sheds. On the ingest route alone it swallows the
  429 the admission bound produces; on the whole control router it puts `/control/changes` behind an
  in-flight bound shared with receipt-blocking ingest handlers — a deny queued behind work of
  unbounded duration, which is exactly the shape the deny lane's separate runtime exists to prevent.
- A **listener-level connection cap** fails the same test one layer lower and worse: declining to
  accept does not refuse a caller, it leaves them in the kernel's accept backlog with no status code
  at all.

## What this does not close

`N` connections still cost `N × ingest_max_batch_bytes` inside whatever bound the deployment
provides. The honest fix is to stream the upload rather than buffer it, which is a change to the
write path's shape and belongs with the flush work, not with a configuration bound.

## Evidence

`crates/tessera-server/src/config.rs` — `INGEST_MAX_BATCH_BYTES_CEILING` and its refusal;
`the_per_connection_batch_ceiling_is_enforced_at_startup` and
`the_default_batch_cap_is_under_the_per_connection_ceiling` pin both directions. The declined
concurrency limit was first assessed at the ingest admission work and is unchanged by this.
