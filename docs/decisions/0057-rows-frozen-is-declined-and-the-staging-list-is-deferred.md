# 0057 — Rows-frozen is declined, and the flip's staging list is deferred

**Date:** 2026-08-07 · **Status:** Settled (owner ruling)

## Context

`compaction.md` has stood **Provisional** since it was written, and by 2026-08-07 everything it
specifies is built except three things: two of them are the subject here, and the third is §6.1's
pair of page-cache hints. The document's own status line named the two as what stands between it and
normative — *"the invariants lens on the staging list of §6.2, if it is built, and on D5's
rows-frozen safety claim, which has never had one"*.

Both are about **machinery that does not exist**, which makes "run the lens" the wrong question. The
right one is whether either will ever be built.

## The decision

**Rows-frozen is declined, not deferred.** D5 ruled the row-space fold primary on 2026-08-05 and
`compaction.md` §13 kept the alternative "recorded, not built" for a deletion-heavy deployment that
wants retirement without the flip. It is now declined outright: nothing will be built on it, and its
safety claim therefore needs no lens.

**The staging list is deferred to a future optimisation**, on the ground of work rather than of
doubt. Decision 0053 already licenses it as best-effort and requires the invariants lens *before* it
is built; that condition survives this ruling and attaches to the future work rather than to
`compaction.md`.

**With both scoped out, nothing blocks the document's promotion** but the owner's word.

## Why

**Rows-frozen was already answered by what it gives up.** D5's deciding argument was that the two
are not two implementations of one operation but two products: rows-frozen delivers retirement and
reclamation with an almost invisible flip and **no read improvement at all**, where the row-space
fold resets segment count from ~152 to 1 at 10⁹ — a ~73 ms saving on a 300-tile viewport against a
135–164 ms baseline, which decision 0049 showed merge cannot deliver on its own. Keeping it
"recorded" cost nothing while it was one paragraph; it stopped being free when it became a standing
obligation on a document that is otherwise finished. **Its enabling property stays written down** —
that removing an entity's postings suffices for invisibility and removing its row is only
reclamation — because that is why the two modes could differ at all, and it is the reason decision
0048's subtraction-only pass is safe. What goes is the mode, not the observation.

**The staging list is four pieces of machinery and a review round, for a latency spike on a nightly
event.** Decision 0053 already made a fold's aftermath a cache miss rather than a refusal, so what
the list would remove is the *first* request per session after a fold — a measured 10.7 s end to end
at 10⁹, once per session, on an operation floored at one a day. Against that, 0053's own shape
specifies: a second `RowProjectionCache` with its own byte budget, plumbed through the engine and
the executor; a most-recently-used filter over *recently-active sessions*, which nothing tracks
today; precomputation during the fold, on a thread the design has not chosen (the fold's own is
sequential by design and the pool is request-serving); and a coverage check at the swap. That
ordering — cheap benefit, expensive machinery — is the deferral's whole argument.

**And the coverage check is the part that makes "quick" the wrong word.** `session_geometry`'s
rung 1 returns a `Peek::Ready` entry *without* checking `extends_to`: an entry under the live key is
assumed to be over the live row space. A precomputed entry built before a flush landed does not
cover the extents carried forward at the flip, so serving it answers an **incomplete mask — items
missing, no error**. A wrong answer wearing a legitimate state's clothes is not a thing to fit into
a spare afternoon, and 0053 was right to gate it on the invariants lens.

## What was rejected

**Running the lens on both anyway, to clear the status line.** A review of unbuilt machinery
produces findings against a design nobody is implementing, and the corpus already carries the cost
of that pattern — review rounds that add mechanism instead of removing it. If the staging list is
ever built, the lens runs then, against the code.

**Keeping rows-frozen as a deferred design.** `docs/design/` distinguishes deferred documents from
provisional ones and this would have been a third thing: an alternative *inside* a normative
document, with an unreviewed safety claim, indefinitely. The two deferred designs in the corpus are
whole documents with their own status lines; a paragraph is not that.

## Consequences

- `compaction.md` §13 records rows-frozen as **declined** rather than retained, keeping decision
  0048's enabling observation and dropping the mode. Its status line loses both blockers.
- `compaction.md` §6.2 marks the staging list **deferred** rather than *"licensed and not built"*,
  with the trap kept at the site — it is the first thing a future implementer must read.
- Decision 0053 is unchanged: the staging list was always optional there, and its *"not built
  without the invariants lens"* condition now attaches to work that has an owner and a date rather
  than to a document that does not need it.
- **Promotion to normative is available and is not taken here.** It is the owner's to confer.
