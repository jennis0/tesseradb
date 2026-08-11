# Streaming the viewport answer: the client's case, and what "good" looks like

**Status:** Handover. Written by the client-side work for a server-side agent to validate and
experiment against. Supersedes `2026-08-11-coarse-first-serving.md` — id-order streaming
subsumes the coarse prelude. The measured figures are from `?trace=1` sessions against the
1e9 fixture, 2026-08-10/11 (traces in repo root; stage table in
`docs/evidence/memos/2026-08-10-viewer-pipeline-cost-model.md`).

## Why the client wants this (the evidence)

A viewport answer at the 1e9 operating point is a **19–42 MB monolith** (measured per arrival,
full/heavy principals, depths 9–10, ~1.2 × 10⁶ points). Consequences, all measured:

- Every frame-rate dip in an otherwise-smooth session (9–21 fps for seconds) coincides with an
  arrival: decode, absorb, buffer-build of one block. At rest the same client renders 4 × 10⁶
  marks at 60 fps — **load is the only remaining hitch**.
- First correct pixels on novel ground: 350 ms p50 at best, 2.5–8 s on the big arrivals —
  against 25–145 ms of server time. The user pans a screen ahead of the data.
- The client has accreted a ledger of coping machinery that is, in sum, a reconstruction of
  streaming semantics client-side: request piece-splitting, two decode lanes, absorb slicing,
  piece-by-piece painting, and (proposed, shelved) byte-capped pieces and storm-deferred
  buffer rebuilds. Each exists because the wire delivers everything-at-once.

## The invariant argument — why Tessera is unusually suited to streaming

`served(T)` is an ascending-`tessera_id` prefix of `vis(T)`, and prefixes nest across depth
(architecture §7.2). The client is already bound to draw only id-order prefixes of what it holds
(`delta-serving.md` §7). Therefore:

> **A response streamed in ascending `tessera_id` order across the viewport is drawable at
> every byte-prefix.** At any cut point, each tile's received marks are an id-order prefix of
> its served set — exactly the state the client is licensed to draw. No phases, no coarse/fine
> seam: pop-in becomes spatially uniform densification of the whole viewport.

Nothing about selection changes. I7 is evaluated exactly as today; the stream is a
**serialisation order**, not a selection rule. The server already holds per-tile selections;
id-major output is a k-way merge over their heads.

## What "good" looks like, from the client

Acceptance criteria the client would build against — each testable:

1. **Keys and counts before marks.** `identityKey`, `contentKey`/ETag, and the per-tile count
   stream (`visible`, `matched`, `served` per tile) arrive in the first flush, before point
   data. The number channel is exact from the first paint; the client also learns each tile's
   final `served`, so progress and completeness are known throughout.
2. **First flush fast.** Target: first drawable flush on the wire within ~50 ms of the server
   starting to serialise; client first-paint on novel ground ≤ 150 ms total (vs 350 ms floor
   today). This is the headline number the experiments should bound.
3. **Flush cadence.** ~1–2 MB or ~100 ms, whichever first; each flush self-contained
   (length-prefixed — §8.6(2)'s owner-annotated framing item — and decodable alone, so the
   worker decodes incrementally and absorb stays sliced as today).
4. **Ordering.** Preferred: global ascending `tessera_id` (uniform densification; the
   every-prefix-drawable property above). Acceptable alternative if the merge is expensive:
   tile-major chunks ordered centre-out — kills the monolith and the hitches, keeps pop-in
   tile-shaped. The experiments should price both.
5. **Mid-stream cancellation.** The client aborts constantly (pans supersede). Dropping the
   connection must stop server serialisation promptly (the D-C cancellation posture today),
   and a truncated stream must leave the client holding a valid prefix — which property 4
   gives for free.
6. **Backpressure.** If the client stops reading, the server must not buffer unboundedly;
   TCP backpressure or an explicit window is fine. State what the implementation does.
7. **Accounting intact.** Server timings and stage figures arrive (trailer or final frame);
   per-response byte totals remain derivable client-side.
8. **Uniform across request kinds.** The anticipation ring and the depth-shadow layers use the
   same verb; streaming must not be viewport-only.

## What the client deletes and builds in return

Under (4-preferred): piece-splitting and its centre-first ordering go; the coarse-first memo's
prelude goes; stand-in→exact pop transitions on novel ground go (ground densifies in place).
The client builds incremental frame decode (the worker's contract becomes per-flush) and keeps
absorb slicing, band replacement-by-longer-prefix (already the replica's model), and the
progressive paint path unchanged — that half of the coping ledger *is* the streaming client.

## Experiments requested

- **E1 — merge cost.** Id-major k-way merge over per-tile selections at the op point
  (~36k tiles, ~1.2M points): serialisation CPU and peak memory vs today's tile-major write.
- **E2 — time-to-first-flush.** Can serialisation begin before selection completes (stream
  during select), or only after (select then stream)? Measure both if both are plausible;
  this bounds criterion 2.
- **E3 — flush cadence sweep.** 0.5 / 1 / 2 / 8 MB flushes: client-visible first-paint and
  total-completion curves at the op point.
- **E4 — the alternative.** Chunked tile-major centre-out: implementation delta and the same
  measurements, for an honest comparison.
- **E5 — abandonment.** Server CPU spent on streams cancelled at 25/50/75% — the pan-heavy
  session's real cost.

## Constraints (not negotiable from the client side)

No invariant is touched: selection, masking, counts-exactness, fail-closed refusals are all as
specified; the stream is serialisation only. Wire changes ride the pre-release window (0048) —
this is the argument for deciding *now*: the change is free today and a per-language port
forever after. `seg_id` stability, dictionary extents, and the running-process format rules
(0048's own carve-outs) are unaffected.
