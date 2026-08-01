# 0038 — Crash realism reads the WAL's sync sidecar; `fsync_offset()` is not built

**Date:** 2026-08-01 · **Status:** Settled

## Context

Conformance §5's crash-realism variant is the test that makes durability *ordering* falsifiable:
after a kill, truncate the WAL to its last-synced offset — simulating the loss of unsynced writes —
and assert no acked operation is missing. A SIGKILL alone loses nothing, because the page cache
outlives the process, so an engine that acked before it fsynced passes a kill-and-restart test.

To supply that offset, §5 specifies `fsync_offset()`: one of three introspection commands, exposed
as a feature-gated RPC behind a `conformance` cargo feature, which also forces the two-binary split
the design documents as a limitation.

## Decision

**Read the sidecar. Do not build the command.**

The WAL already publishes its last-synced position durably, because replay needs it: `<name>.sync`
holds an 8-byte little-endian offset, written write-tmp-then-rename and fsynced together with its
directory entry after every WAL fsync. The harness reads that file and truncates to it.

The reason is that **the number is already on disk**. A command would have added a feature-gated
introspection surface — and the `conformance` feature and its two-binary split with it — to expose a
value any process can read.

The path derivation is transcribed into the harness rather than obtained from the server, which
keeps that much out of the engine's hands.

## The second reason this decision originally gave was wrong

The first draft argued that a command "would have had the engine report on the property under test",
and that the sidecar is "closer to reading the disk than to asking the engine". **That is false, and
it was refuted by measurement rather than by argument.** Replacing `self.file.sync_data()` with
`Ok(())` in `Wal::sync_and_publish` — so the WAL is never fsynced, while the offset is still
published and acks still return 200 — leaves both tests in
`conformance/tests/test_restart_replay.py` passing.

The sidecar *is* the same component's bookkeeping, merely persisted. Reading it establishes what the
engine claims is durable, not what is durable. So the choice between a sidecar and a command was
never a choice about evidential strength: **neither can establish ack ordering**, and the decision
rests on the first reason alone, which is a decision about machinery rather than about proof.

This correction is recorded here rather than in a superseding decision because the decision itself
— read the sidecar, do not build the command — is unchanged. What changed is one of its stated
reasons, and leaving a refuted argument in place would let the next reader inherit it.

## Consequence

**Durability ordering remains unverified end to end**, and conformance §5's crash-realism paragraph
now says so instead of claiming otherwise. It is held by the write path's `Published` token type and
the fault-injection pause site inside the ack function (lifecycle §4, §7.3) — real evidence, in
Rust, of a narrower property. Issue #71 asks whether an end-to-end check is worth its cost; the
routes that could work (syscall observation, a fault-injecting filesystem) are both outside what the
suite can do on a hosted runner.

`fsync_offset()` leaves §5's command list, which is now two: `evict_fragment(key)` and
`ledger_state()`. Neither is reachable from the suite, and `ledger_state()` cannot be built at all
until the ledger it would report exists.

**The `conformance` feature is still required — for the interleavings, and for nothing else now.**
The eight pause points need it. §5's warning stands undiminished: they must be built by extending
the write path's existing fault switchboard (lifecycle §7.3), not beside it.

This decision does not generalise to "tests may read engine internals". The sidecar is a documented
on-disk format with a stated contract, which is what makes it readable from outside; an internal
data structure would not be.

## Evidence

`conformance/tests/test_restart_replay.py`; `crates/tessera-lifecycle/src/wal.rs`, "the durable
prefix"; conformance design §5 and its r6 markers.
