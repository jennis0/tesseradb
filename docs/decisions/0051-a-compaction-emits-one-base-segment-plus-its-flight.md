# 0051 — A compaction emits one base segment, plus whatever its flight published

**Date:** 2026-08-06 · **Status:** Settled (owner ruling)

## Context

`contracts.md` §2.1 said:

> `tessera build` and every compaction emit exactly one segment per partition-slice — a build *is* a
> full compaction.

A fold cannot satisfy that, and the reason is a property nobody wants to give up: **it never blocks
flush**. A fold runs for minutes to hours over the whole corpus; flushes publish into the old prefix
throughout and are carried forward at the flip with a new `row_base`. So a fold ends with one base
segment plus a tick's worth of extents — never exactly one.

Found by `compaction.md`'s r3 adversarial round, and not that document's to resolve: `contracts.md`
is interchange specification.

## The decision

**The sentence is narrowed** (contracts r20). A compaction emits exactly one **base** segment per
partition-slice, plus whatever extents were published during its flight.

## Why

The property the sentence protects is that the slice-level `permutation.bin`'s single-segment
addressing (§2.6) is sufficient. That property is unharmed, because it was always addressing the
**base**: carried-forward segments are extents carrying their own row maps, addressed exactly as
flush segments already are between compactions. Nothing about the format, the readers, the writers
or the oracle changes — which is why this is a narrowing and not a `bundle_format` bump.

## What was considered and declined

- **Block flush for the fold's duration**, making the old sentence true. Declined: it contradicts
  `compaction.md` §1's *"ingest, denies and flush continue"* and SA §6.7's *"flushes never block"*,
  and it converts the most expensive maintenance operation in the system into a write outage of the
  same length. Decision 0043 — maintenance never blocks a request — settles this directly.
- **Have the fold re-fold its own in-flight extents just before the flip.** Declined: the race does
  not close, it shrinks. A flush can publish between the re-fold and the `CURRENT` flip however
  short the window, so the carry-forward path is needed anyway — and then it is a second mechanism
  earning nothing.
- **Emit one segment and drop the flight's extents.** Not seriously considered; it discards
  acknowledged writes. Recorded because it is the reading the old sentence most literally invites.
