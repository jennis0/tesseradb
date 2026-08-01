# Tessera white paper — design

**Date:** 2026-07-28
**Status:** approved design, ready for implementation planning

## 1. What this is

A public-facing explanatory white paper on how Tessera works: a single
self-contained interactive HTML page, published as an Artifact, that takes a
technically-literate reader with no database-internals background from "why is
this hard" to "I understand the mechanism".

**Audience.** A public technical reader — HN, a portfolio, a blog. Not a
collaborator onboarding to the codebase, not a buyer, not an internal
reference. The success criterion is that a reader who knew nothing about
permission-masked aggregates finishes the paper understanding why they are
hard, why nobody serves them at scale, and how Morton ranking plus Roaring
bitmaps solves both halves.

**Scope.** The whole system: read path, write path, lifecycle, limits. Not the
crate layout, not the byte formats, not the plugin ABI — those are contracts,
and this paper is about mechanism.

**Non-goals.** It is not a specification, does not supersede any document in
`docs/design/`, and does not document the API. It does not attempt to teach
Roaring bitmaps or Morton curves in general — only as much as the argument
needs.

## 2. Framing decisions

**Tense and status.** Present tense throughout ("a tile is a contiguous row
range"), with a single clear status note near the top: this is the
architecture as specified, Phase 0 measured on a synthetic-policy corpus
scaled to 10⁹, Phase 1 in progress. No per-sentence hedging. The paper reads
as a systems paper, not a proposal.

**The thesis has two axes, not one.** Aggregation *and* scale, presented as
co-equal:

- *Aggregation.* Every surveyed system with per-document security permits
  aggregates over records the viewer cannot read. The field draws its line at
  retrieval and lets everything derived leak past it.
- *Scale.* Nobody serves 10⁹ identifiable, filterable, labelled points at all
  — with or without masking. Systems reaching 10⁷ do so by fixing the sample
  before any user exists, or by shipping every point to the client. Systems
  reaching 10⁹ do so by binning to rasters, which discards identity.

These two axes are independent, which is what makes the target region empty
rather than merely unoccupied.

**The ending is the honesty.** The paper closes on what was measured and what
remains open — including the leak register's sixteen accepted residual
disclosure channels. A paper that opens by pointing at an empty quadrant and
closes by naming which parts of it are still unproven is stronger than one
that closes on a benchmark.

## 3. Structure

Six parts, ordered so a reader who stops after Part III has still got the
point.

### Part I — The empty quadrant
1. **Three viewers, one viewport, three different maps.** The thesis shown
   before it is stated.
2. **Where the field draws the line.** A count over records you cannot read is
   a disclosure, not a filtered view. Filtered aggregates leak.
3. **And the ceiling.** The scale half: nobody does this at 10⁹ at all.

### Part II — The shape of the data
4. **Two coordinate systems.** Entity space for permissions, row space for
   geometry, related by an explicit permutation.
5. **Morton ranking.** Why a square on screen becomes a contiguous range of
   rows.
6. **Roaring, and the cost model.** Cost is *containers touched*, not
   cardinality — and therefore cost scales with screen area, not corpus size.
   This is the scale answer, and the whole design is written against it.

### Part III — Turning permission into arithmetic
7. **The plugin boundary.** Grants → boolean expression → terms → postings →
   one bitmap, built once per session and reused.
8. **Signature-sorted entity IDs.** The measured compression, and why it can
   never be retrofitted (I9 makes the assignment permanent).
9. **A viewport query, end to end**, in five steps.

### Part IV — Making a map out of a bitmap
10. **Level of detail, and sampling after masking.** Priority nesting, direct
    evaluation as the main selection route. I7: sampling happens after
    masking, and the consequence of getting it backwards is that the sparsest
    principals' maps go blank silently.
11. **Labels, clusters, and the containment frontier.** Labels gate on
    `M_auth`; filters may move the frontier up, never down.
12. **Two masks.** Filters narrow points without dissolving the map.
13. **Compartmented partitions and required-set gating.**

### Part V — Time and change
14. **Ingest.** WAL and the ack contract, buffer / watermark / overlay,
    segments, merging and compaction.
15. **Denial.** Generations, pins, and the three retirement rules — deletion
    by epoch ledger, suppression only on unsuppress, predicate-change at the
    compaction fold. Conflating them is fail-open. Pins fix geometry, never
    authorisation.

### Part VI — What was measured, and what is still open
16. **The billion-row table.** Exact masked counting at 0.1–0.3 ms across
    every zoom depth, plus the counterweight: permuting a 69M-item mask into
    row space costs 8.8 s, which is why it is cached per (token, slice, pin)
    and must never reach the per-viewport path.
17. **Still open.** Real-label policy shape, sharded placement, retroactive
    revocation, and the leak register C1–C16.

## 4. Figures

Sixteen figures — nine interactive, one animated, six static. All hand-authored inline SVG plus a little
canvas — the Artifact CSP blocks external hosts, so no charting library, no
CDN, no remote fonts. Every figure works in both light and dark themes. Wide
figures scroll inside their own `overflow-x: auto` container; the page body
never scrolls horizontally.

| # | Figure | Mode | Part |
|---|---|---|---|
| F1 | Three viewers, one viewport | interactive | I |
| F2 | Where the line is drawn — two query plans | static | I |
| F2b | **The empty quadrant** — surveyed systems on scale × granularity | interactive | I |
| F3 | Two coordinate systems and the permutation | static | II |
| F4 | **The Morton explorer** | interactive | II |
| F5 | **Roaring anatomy** — containers touched vs cardinality | interactive | II |
| F6 | Grants become a bitmap | interactive | III |
| F7 | Signature sorting, before and after | static | III |
| F8 | **One query, five steps** | interactive | III |
| F9 | Sampling after masking (I7) | static | IV |
| F10 | The containment frontier | static | IV |
| F11 | Two masks | interactive | IV |
| F12 | Compartments and required sets | static | IV |
| F13 | The write path, on a timeline | animated | V |
| F14 | **Three retirement rules** | interactive | V |
| F15 | The billion-row table | static | VI |
| F16 | The leak register, C1–C16 | interactive | VI |

**Load-bearing figures** (bolded above): F2b, F4, F5, F8, F14. If effort has
to be cut anywhere, it is cut from the others first — these five carry the
argument.

### Figure notes

- **F1** — one map, a persona switch between three viewers. Points, counts,
  cluster bubbles and density all redraw together. The reader sees that
  *every* derived quantity changes, not just which dots are present.
- **F2b** — x-axis: demonstrated interactive scale, log, 10³ → 10⁹. y-axis:
  access-control granularity (none / dataset / row). Points carry citations on
  hover. Systems reaching 10⁹ by binning are marked with an asterisk that
  reads "identity discarded". The target region is top-right and empty.
- **F4** — drag and resize a viewport over a Z-curve; the decomposition into
  contiguous row ranges updates live. A toggle switches to row-major layout so
  the range count visibly explodes.
- **F5** — a bitmap shown as 2¹⁶-chunk containers, each labelled array /
  bitmap / run. Intersect two masks and read the counter: containers touched
  against total cardinality. This is where the reader internalises the cost
  model, and it is the figure the scale claim rests on.
- **F14** — one scripted interleaving with a stepper, plus a "conflate the
  rules" toggle that walks the reader into the fail-open.
- **F16** — filterable table, presented as a feature of the design rather than
  an appendix to it.

## 5. Accuracy requirements

The paper makes public claims about third-party systems and about measured
performance. Both must be exact.

- **Every competitor claim carries a citation** traceable to
  `docs/evidence/prior-art/prior-art-*.md`, which in turn cites primary sources. Claims about
  demonstrated versus marketed scale must preserve that distinction — for
  example, deepscatter's billion-point artefact is a *static* star catalogue,
  and saying otherwise would be false.
- **Every performance number is traceable to `probes/results.md`** with its
  measurement conditions attached. The environment (WSL2, 12 cores, 39 GB) is
  stated once; timings are described as indicative, matching the memo's own
  characterisation.
- **The synthetic-policy caveat is stated plainly**, not buried: Phase 0 ran
  synthetic policies over a real 2.42M-paper arXiv corpus, scaled to 10⁹.
- **Invariant statements must match §4 of the architecture design verbatim in
  substance.** I2, I3, I7, I9, I10, I12 and the three retirement rules all
  appear in the paper; misstating any of them publicly is the worst available
  failure mode.

## 6. Production plan

1. Load the `dataviz` and `artifact-design` skills before writing anything, so
   all sixteen figures share one palette, one type scale and one interaction
   grammar. Sixteen improvised diagrams is the primary failure mode.
2. Dispatch one fact-extraction subagent per part. Each reads its governing
   documents and returns a cited fact sheet: claim → document → section. They
   extract; they do not write prose.
3. Write all prose and all figures in a single hand, for one voice and one
   visual system.
4. Independent review before publishing: a subagent checks the finished paper
   against the design corpus and the thirteen invariants, with no stake in the
   paper being right. Act on that review before publishing.
5. Source file lives in `docs/`; publish via the Artifact tool. The repository
   is not under git, so the file is simply written, not committed.

## 7. Constraints

- Self-contained: no external scripts, stylesheets, fonts or images. Assets
  inlined or embedded as data URIs.
- Theme-aware: correct in light and dark, honouring both
  `prefers-color-scheme` and an explicit `data-theme` override.
- Responsive: relative units, no horizontal body scroll, figures degrade
  legibly on narrow screens.
- Target length: roughly 8,000–10,000 words of prose plus the figures.
