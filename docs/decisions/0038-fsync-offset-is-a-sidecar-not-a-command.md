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

Two reasons, and the second is the stronger one:

1. The number is already on disk. A command would have added a feature-gated introspection surface,
   and the `conformance` feature and its two-binary split with it, to expose a value any process can
   read.
2. **A command would have had the engine report on the property under test.** The question is
   whether the engine's acks lag its fsyncs; asking the engine where its fsyncs got to answers it
   with the same component's own bookkeeping. The sidecar is the durable artefact the engine's
   recovery path is itself obliged to honour, so reading it is closer to reading the disk than to
   asking the engine.

The path derivation is transcribed into the harness rather than obtained from the server, for the
same reason.

## Consequence

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
