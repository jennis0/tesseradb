# Phase 1 handoff brief

**Written 2026-07-27, at the end of the Phase 0 measurement work.** Its
job is to let a fresh session scope Phase 1 from documents rather than
from a transcript. If this brief and a source document disagree, the
source document is right.

---

## 1. Where the project actually is

Phase 0's measurement work is **complete**; its memo is written and has
been through independent review. The verdict is **GO — scoped**, and the
scoping matters:

> Go for Phase 1 spend. **The plan's Phase 0 gate itself stays open.**
> Plan §4 requires real predicates and real grant sets; this work used
> synthetic policies over a real corpus (2.42M arXiv papers). The
> real-label rerun retains full **go / rework / stop** authority — it is
> not parameter tuning.

Phase 1 must therefore be scoped as *a walking skeleton over a term-index
architecture that is measured-plausible but not finally validated* — not
as though the risk is retired. Nothing in Phase 1 should be hard to undo
if the real-label rerun says rework.

No code exists. There is no Cargo workspace and no git repository. Do not
scaffold one without being asked.

## 2. Reading order

1. **`probes/phase0-memo.md`** — start here; it points at everything else
   and states the scoped verdict.
2. **docs/archive/implementation-plan.md §5** — Phase 1 as originally
   scoped (walking skeleton, exit criteria).
3. **`docs/design/system-architecture.md` (r4)** — §3 crate
   decomposition, §4 the five contracts, §6 lifecycle. This is newer than
   the implementation plan and **amends it**; see §4 below.
4. **`docs/design/architecture.md` §2.6 and §4** — the request
   path end to end, and the thirteen invariants. Read §4 rather than
   working from memory of it.
5. **`probes/optimisations.md`** — the engineering distillation: what
   the measurements imply for what gets built, each item labelled
   DECIDED / PROPOSED / DEFERRED. Read this before scoping tasks.
6. `probes/pre-phase1-verifications.md` — croaring/maturin/PyPI findings
   and the project framing line.
7. `probes/dataset.md` and `probes/results.md` — the corpus and the full
   measurement record; consult when a specific number is disputed.

## 3. Decided — do not reopen

Each of these is recorded with its evidence in the documents above.
Re-litigating them is the main way this handoff can waste time.

| Decision | Where argued |
|---|---|
| Authorise path is the **union of per-term postings**; the pair-relation semi-join is build-cadence machinery and the differential oracle (a reassignment from plan §4.2, made on measurement) | memo §2.2; results.md §4 |
| **Direct evaluation is the main selection route**; candidate lists serve only dense regions of high-coverage principals. The exact path is load-bearing for sparse principals and must not be "simplified" away | memo §2.2; results.md §5 |
| **Signature-sorted entity-ID allocation belongs in the Phase 1 allocator.** I9 makes assignment order permanent, so created-order in Phase 1 locks in an uncompressed index until a full re-allocation rebuild. Measured worth: 8.9–36.7× on postings, 6–1,614× on the cached session fragment | results.md §4.4 |
| **Tile → *set of ranges*** in the tile-table and count-path interfaces (§11.3's multi-segment reality forces this regardless; it also keeps the signature-aligned layout available later) | results.md §4.2, design §11.3 |
| **Frozen mask buffers must be 32-byte aligned**, exact-sized — a bundle-format requirement, not an implementation detail | pre-phase1-verifications §1 |
| Distribution name **`tessera-index`**; import name and CLI binary stay `tessera`. Reserve with a placeholder release | pre-phase1-verifications §2 |
| Framing line: *"A scalable, interactive backend for ordered 1D and 2D data, with per-item access control applied to counts, densities, hierarchies and labels — not just to retrieval."* | pre-phase1-verifications |
| **Row-space signature-major layout is deferred** to post-conformance-suite, per deployment. Its whole-group visibility shortcut is invariant-bearing; its costs are tile fan-out (~6–8× segment count) and policy-coupled physical placement | results.md §3 |

## 4. Amendments Phase 1 scoping must fold in

The implementation plan predates architecture r4. Where they conflict,
reconcile explicitly rather than silently:

- **`tessera build` is Rust, not Python** (r4 D4, amended). This
  *dissolves* the dual-tiler bit-for-bit obligation that r3 created — do
  not scope a Python bulk tiler or its differential test.
- **The WAL joins the walking skeleton's storage work** (r4 Appendix R
  action 1); r1's plan had no durability story. An unpersisted overlay
  fails open.
- **Conformance matrix gains tests** (r4 Appendix R action 2): the
  restart-replay test for deny-disposition overlay entries, and the
  deny-retirement window test.
- The two-plane split (control vs viewer) and the process model (single
  process when no compartments exist) are r4 §1–§2; Phase 1 is
  single-partition, so the router/worker machinery stays dormant.

## 5. Open — and *not* for Phase 1 to settle

Retroactive revocation across views (design §9, architecture-affecting);
sharded index placement (§13.3); prompt-sample vs full-membership label
gating (§7.8 — Phase 3, recorded in the manifest either way); entity-ID
width and exhaustion (§16 — note the new input that croaring's frozen
views are u32-only, which argues for keeping cached fragments 32-bit);
overflow-item visibility (§6.2).

## 6. Known gaps in the tooling, inherited

- No `croaring` binding for `roaring_bitmap_range_uint32_array`
  (design §10.4 names it). Workarounds: range-AND then `to_vec`, iterate
  the view, or a two-line FFI shim. Phase 2 detail, not a redesign.
- `Bitmap64` has no frozen-format support and no `or_many`.
- Frozen *sizes* were never measured (pyroaring exposes only the portable
  format); measure them once the Rust side exists.

## 7. How to proceed, per the project's working method

Write the Phase 1 plan first, then **hand it to a subagent for
independent review against the design documents and §4's invariants,
with no stake in the plan being right.** Act on that review before any
code is written. That shape caught a blocker in the Phase 0 memo (an
unscoped GO that would have dissolved the plan's own gate), so it earns
its cost.

Decompose the implementation itself across subagents and review what
comes back against the spec; invariant-bearing decisions stay with the
reviewer, not the worker.

## 7a. The scaled test corpus (added after this brief was first written)

`data/scaled/` holds **one 10⁹-point corpus whose prefixes are smaller
corpora** — 250k / 2,422,486 / 250M / 1B, selected by filtering
`entity_id < limit`, with the 2.4M prefix bit-identical to the hashed
geometry artifact. Full write-up and numbers in `dataset.md`.

Phase 1 relevance: the plan's §5 walking skeleton calls for 10⁷
synthetic points; this supersedes that with something better shaped
(density spanning 4–5 orders of magnitude, deliberate edge cases, a
term vocabulary mixing corpus-spanning and locally-clustered postings),
and it gives the "prove 10⁸ before saying 10⁹" checkpoint its corpus
already. A masked 300-tile viewport at the full 10⁹ scale measures
~1–2 ms of exact `range_cardinality`, so the design's core claim is now
demonstrated on a real 10⁹ Morton ranking rather than argued from
Appendix A. Note the permutation into row space costs seconds at that
size — cache it per (token, view, pin) as §10.4 requires; it must
never sit on the per-viewport path.

## 7b. Findings from the 10⁹ corpus that bear on Phase 1/2 scope

Full detail in `results.md`; three that change what gets built.

1. **Entity-space contiguity is worth up to ~130× on mask build**, at
   identical coverage and identical mask size (21.7 ms vs 2,885 ms at
   ~25% coverage, replica-local vs scattered terms). Together with the
   8.9–36.7× posting compression in the stage 1 addendum, this is two
   independent measurements of the same mechanism — and the strongest
   argument for **signature-sorted entity allocation in the Phase 1
   allocator**, since I9 makes the ordering permanent and retrofitting
   means a full re-allocation rebuild.
2. **The mask-build kernel needs an algorithm choice, not one path.**
   Roaring union costs O(terms × containers spanned); concatenate +
   radix sort + bulk construct costs O(total postings). For many small
   scattered terms the latter wins, and it is the construction §10.4
   already prescribes for the permutation. Phase 2 should implement both
   with a heuristic on postings-per-container spanned.
3. **Dictionary scale is a storage problem, not an authorise-path
   problem** — 117M terms costs the same mask build as 10M. Sizing
   follows §6.2's small-term rule (34% of terms are singletons at that
   scale), not the Roaring-per-term shape.

## 8. Artifacts on disk

```
data/corpus.parquet              2,422,486 rows — the canonical base artifact
data/geometry.parquet            + .sha256 (4a59a9b8…) — the hashed artifact
data/pairs/*.pairs.parquet       label sets, (seed, config), regenerable
data/arxiv_papers_embeds.parquet 4.4 GB source embeddings
data/scaled/                     10^9-point corpus, prefixes = 250k/2.4M/250M/1B
  geometry.parquet               12 GB, sorted by (morton, entity_id)
  pairs.parquet                  5.9 GB, 1.72B pairs, 47,968 terms
  scales.json                    scale limits, row counts, derivation rule
probes/build_corpus.py           dedup + join + entity-ID assignment
probes/gen_pairs.py              label generators (4 configs + knobs)
probes/mask_probe.py             grant grid + mask build (--tile N to scale)
probes/stage2_probe.py           Morton autocorrelation + tile coverage
probes/build_geometry.py         PCA-64 → cuML UMAP → Morton rank → hash
probes/build_scaled_corpus.py    the 10^9 builder (--points N)
probes/verify_scaled.py          prefix fidelity + masked viewport at scale
```

Disk after all of the above: ~98 GB free of 393 GB.

Python env: rebuild with `uv venv --python 3.12` plus duckdb, polars,
pyarrow, numpy, pyroaring, kagglehub, cuml-cu12 (system Python is 3.10).
The Kaggle snapshot is at
`~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/`.
