# 0060 — A streamed response lives at most the whole-stream deadline

**Date:** 2026-08-11 · **Status:** Settled (owner ruling)

## Context

The streamed viewport response (`streamed-serving.md`) produces bytes in step with the client
reading them. Before streaming, a response was computed whole — sub-second — buffered, and the
request's admission slot freed at once; the client's read pace never held server resources. Under
streaming, a slow reader occupies an admission slot (one of `compute_admission + compute_queue`,
144 by default), a blocking-pool thread, and ~two flushes of buffer for the life of the transfer.
The per-send stall deadline (`serve.stream_write_stall_ms`, 10 s) sheds a reader that has stopped
entirely, but not a *dripping* one that reads just enough to reset it — which can hold a slot for
minutes, legally, per stream. The admission slots are also the front door: exhausted slots mean an
immediate 429 for every new viewer-plane request. Decision [0059](0059-per-principal-admission-is-not-capped.md)'s
occupancy reasoning was made against compute-bound holds and never priced a client-paced one; the
design review flagged the gap and the server-reply memo reserved it for this ruling.

## The decision

**Every stream is bounded by `serve.stream_deadline_ms` — 60 s from first flush — whatever the
client is doing.** Together with the stall deadline this caps worst-case slot occupancy at
`slots × deadline`, absolutely. Both knobs are config, refuse zero, and default as named.

## Why

The cost is a refusal of clients slower than the deadline can carry: at the heaviest measured
response (~42 MB at the 10⁹ operating point) the 60 s default cuts readers under ~0.7 MB/s, and a
retry fails the same way. No such client exists — this is pre-release, with no deployment and no
slow links — so the cap currently refuses nobody, while removing an unbounded hold that a handful
of hostile connections could otherwise use to park the server's entire admission capacity.

## What was rejected

**A separate streaming lane** — its own resource pool, so streams never consume admission slots
and could be given unlimited time. The cleanest isolation, and the design to reopen if a real
deployment ever has slow-but-legitimate readers; rejected now as a second bound to size, operate
and review, bought against a client population that does not exist.

**No deadline, with the arithmetic accepted** — coherent for a system with no untrusted clients,
but it makes the admission surface parkable by anyone with a session token and patience, and the
defence would then be retrofitted under pressure rather than designed.

## Consequences

- `serve.stream_deadline_ms` and `serve.stream_write_stall_ms` are policy, not plumbing; an
  operator raising them is choosing a larger parkable surface and should read this file first.
- `/control/status`'s `compute.streaming` gauge is the observability for this posture: slot held,
  compute released, bounded by the deadline.
- Revisited only on evidence of legitimate slow readers, and the revisit's shape is the separate
  lane above, not a bigger number.
