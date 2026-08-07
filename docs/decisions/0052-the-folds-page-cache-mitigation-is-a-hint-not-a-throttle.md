# 0052 — The fold's page-cache mitigation is `MADV_SEQUENTIAL`, not a read throttle

**Date:** 2026-08-06 · **Status:** Settled (owner ruling)

## Context

P3 measured what a corpus-scale streaming read costs a concurrent viewport: up to **2.03×** at a
45.57 GiB bundle against 36.9–38.2 GiB of RAM, monotone in the rate at which bytes move
(`docs/evidence/memos/2026-08-05-compaction-flip-and-io.md`). `compaction.md` §6.1 read that as
setting an IO rate limit and fixed it at 128 MiB/s.

**The r5 review refuted the mechanism.** Every one of the fold's inputs is an `Mmap::map` —
`MortonSlice::load`, `ColumnsRef::load`, `Permutation::load`, the postings reader and every delta
tier — and spec §3 depends on exactly that (*"a cursor into a segment is an index into a mapped
buffer"*). The fold's byte movement is page faults inside load instructions. **There is no read to
sleep between.** P3's probe reads with `File::read` and sleeps between calls, so it modelled the
harm faithfully and the mitigation not at all.

A further correction to the escalation as first drafted: **`posix_fadvise(POSIX_FADV_DONTNEED)`
does not evict mapped pages.** It calls `invalidate_mapping_pages()`, which skips any page with a
live page-table reference — so on the fold's whole-file mappings it would be close to a no-op.

## The decision

**`madvise(MADV_SEQUENTIAL)` on each input mapping, at open.** One call per file. No rate, no
device-specific constant, no tuning.

## Why

**The goal was never to slow the fold down** — its duration is free under §6.1's standing budget.
The goal is that when the kernel reclaims, it takes the fold's pages and not the viewport's.
Unthrottled, it does the opposite: the fold's pages are the most recently touched, so they look
hottest, and the viewport's mapped hot pages are evicted to make room for bytes nothing will read
again. That inversion is the whole of the 2.03×.

`MADV_SEQUENTIAL` states exactly that: the range is streamed, read it ahead aggressively, and the
pages may be freed soon after they are accessed. It is true of what the fold does, it has no
correctness surface, and it costs none of the duration.

## What it obliges

- One `madvise` call per input mapping. `libc` is already a workspace dependency (`tessera-build`
  uses it for `statvfs`, which spec §8's free-space precondition needs anyway).
- **The write side is a separate mechanism and is not covered here.** The fold spools column bytes,
  reads them back through mappings, writes assembled batches, and dirties a 4 GB `permutation.bin`
  mapping. For the spools and outputs — plain buffered writes, *not* mapped — `sync_file_range`
  followed by `posix_fadvise(DONTNEED)` does work and is the standard pattern. `compaction.md` §6.1
  must say so; this ruling does not settle it.
- **P3 re-run with the hint applied** before the mitigation is believed. `MADV_SEQUENTIAL` is a
  hint the kernel may ignore, and nothing measures it here yet. The re-run needs a sweep long
  enough to displace a real fraction of the bundle: the existing throttled arms move under 1% of it
  in ~3.5 s, which cannot distinguish "mitigated" from "hadn't got going yet".

## What was considered and declined

- **`madvise(MADV_COLD)` behind the cursor** — demotes consumed pages to the inactive LRU so
  reclaim takes them first. Sharper, and the right escalation if the hint proves insufficient, but
  it needs the cursors to track consumed ranges, which is new state. Not the starting point.
- **Windowed mapping plus `posix_fadvise(DONTNEED)`** — the only shape in which fadvise fires,
  because unmapping is what makes the pages eligible. Declined: it rewrites `MortonSlice` and
  `ColumnsRef` from whole-file to windowed, and **those types are on the request path**. Surgery on
  the read path to fix a maintenance problem is the trade CLAUDE.md's "an optimisation that costs
  reviewability needs an argument" is written against.
- **Pacing the three streaming producers** (`SegmentWriter`, `coalesce::merge_runs`,
  `PostingsSpool`). Declined: all three are *also* flush's and merge's, so each would grow a rate
  knob that must be off on the request-adjacent paths — and it drags the device-specific number
  back. It also cannot pace `PermutationWriter::create`, which dirties 4 GB in one statement.
- **Accept the 2.03× and mitigate nothing.** A defensible baseline, and cheaper than it looked
  before P3 separated the real figure from the 15.7× cgroup artefact. Declined only because
  `MADV_SEQUENTIAL` costs one call; if the re-run shows the hint does nothing, this is where it
  lands.

## What this supersedes

`compaction.md` §6.1's 128 MiB/s rate, and recommendation 2 of the P3 memo, both dated 2026-08-06
and both withdrawn the same day. The **measurement** they rest on is unaffected: streaming the
bundle past the page cache costs a concurrent viewport up to 2.03×, and that is why a mitigation is
wanted at all.
