# What a fold costs in memory and wall clock — probe P1

**Date:** 2026-08-07 · **Status:** Evidence, never normative
**Subject:** `compaction.md` §14's last modelled figure — the fold's own peak RSS, which §12's
obligation 12 makes a probe rather than a unit test and which §3's budget rests on. Also its wall
clock, which was modelled as "IO-bound, minutes" and is now measured.
**Harness:** `crates/tessera-engine/tests/scale.rs`,
`a_fold_over_a_multi_segment_corpus_at_two_sizes`, `#[ignore]`d so the figures re-run from the tree.
Raw output: `probes/2026-08-07-fold-memory/`.

---

## Results

**The budget's size is confirmed. Its shape was wrong, and inside the wrongness were two defects.**

| | measured | at |
|---|---|---|
| peak resident set, whole fold | **0.93–0.99× the bundle's live bytes** | 2.5–10×10⁷ rows |
| of which **file-backed** — mappings, reclaimable | **~92%** | |
| of which **anonymous** — what the node cannot give back | **5–8 B/entity** | |
| the publication's half of the peak | **~10%** of it | |
| wall clock | **~0.39 µs/row**, linear across a 4× range | |

**Projected to 10⁹ entities: ~40 GB resident, ~7.1 GB anonymous.** Add §3's dictionary term — 8 B ×
1.17×10⁸ ordinals ≈ 0.94 GB, which this fixture's two-term dictionary cannot exercise — and the
un-reclaimable total is **~8 GB against §3's stated ~9–10 GB**. The budget is right. What was wrong
is what it was a budget *of*.

**Wall clock at 10⁹: ~6.5 minutes**, from 0.39 µs/row measured at three sizes on this device.
§14 recorded this as modelled and now has a number, with the usual caveat that it is one device's.

## What P1 found, which is why it exists

§3 forbids the fold to inherit any construction that decodes its input at once — the merge's
measured 4.4–4.9× multiplier applied to a corpus is 200+ GB on a 47 GB bundle. Two places broke that
prohibition.

**Pass 5 read every file it had written whole into a `Vec<u8>` to hash it.** `digest_of` was
`fs::read` followed by `Sha256::digest`. Bounded for a flush, whose segment is one commit window,
and bounded for a merge, which `max_merged_segment_bytes` caps at 256 MiB. For a fold the operand is
the corpus's own `columns.arrow` — tens of GB at 10⁹, in one allocation, inside the serving process.
**There were three copies of that function** (`tessera-engine::flush`, `tessera-engine::coalesce`,
`tessera-store::flush`), all reading whole, so a fix applied where the problem was found would have
left two. There is now one streaming definition in the store and the other two delegate.

**Pass 3 decoded every external-id run into the heap.** `RunCursor::open` read the run through
Arrow's `FileReader` and collected every batch — the whole corpus's external ids, in anonymous
memory, which is where the fold's peak actually was. The *query* path never had this problem:
`sidecar::load_validated` has always mapped the same file and decoded it zero-copy. This was the
maintenance path not using the reader that already existed.

**Neither was visible to a total-RSS figure.** Fixing them moved the anonymous term from 255 MB to
83 MB at 10⁷ and moved the total by 0.04×, because a fold that maps everything correctly still reads
near 1× as its inputs and outputs become resident page cache. That is why the probe asserts on the
anonymous half: a bound on the total would have passed throughout.

**Finding pass 3 needed the peak's *timestamp*, not its size.** The fold's own staircase
(`compact::PassCost`, sampled at pass boundaries and published on `/control/status`) showed the
resident set climbing 57 MB across the whole fold, because the 445 MB peak was intra-pass and came
back down before pass 3 returned. The sampler's `at t=` is what located it.

## The two figures, and why quoting one is a mistake

**A node's RSS during a fold is ~1× its bundle's live bytes.** Every input and output is a mapping,
so the pages the fold touches are resident while it touches them. This is the number an operator
watching a process will see, and it is *not* a budget: `MemAvailable` already counts reclaimable
page cache as available, so a pre-flight charging the fold for it would refuse every fold on a host
whose bundle exceeds RAM — which is every host this design is for.

**What the node cannot give back is 5–8 B/entity.** Clean mapped pages are reclaimable by
definition. What is not is the anonymous half plus the dirty pages of the two arrays the fold
*writes* through a mapping (`permutation.bin`, `ext-locator.u32`, 4 B/entity each until writeback).
That is the quantity §3 budgets and the quantity the pre-flight now compares.

**"Peak RSS is flat in corpus size" was never true and is not the property to want.** §12's
obligation 12 read that way; §3's own table has always said otherwise, listing terms that scale with
entity space and with the widest term's coverage. The measurement settles it: the anonymous term is
linear in entity space at a coefficient of bytes. What distinguishes a streaming fold from a
decoding one is the *size* of that coefficient, not its absence, and the obligation is reworded to
match.

## What this does not measure

- **The dictionary term.** The fixture has two terms. §3's ~0.94 GB at 1.17×10⁸ ordinals is
  unexercised, and the projections above are the entity-space half only.
- **The widest term's encode.** Same reason. §3 models ~375–500 MB at 10⁹ from an independently
  measured 125.12 MB per 25% grant (`probes/results.md` §4.2); nothing here constrains it.
- **A fold at 10⁹.** The largest run is 10⁷ and the projection is linear extrapolation across a 4×
  range. What the linearity does establish is that no term grows faster than the corpus, which is
  the claim a pre-flight budget needs.
- **A fold under concurrent load.** P1 runs against an idle engine with the background refresh off,
  so the flip's own cost is P2's subject and the page-cache contention is P3's. This measures the
  fold.

## Recommendations

1. **§3's budget stands and its shape is corrected** — un-reclaimable rather than resident, and
   linear in entity space rather than independent of rows. Done in this pass.
2. **The pre-flight refusal is built to the corrected budget**: 2 × (4 B × permutation bound + 4 B ×
   entity bound + 8 B × dictionary length), against the smaller of `MemAvailable` and the cgroup
   `memory.max`. The doubling stands in for the widest term's encode, which needs a postings scan
   the planner has no reason to do and whose modelled magnitude sits far inside the allowance.
3. **Re-run P1 at 10⁹ before quoting the projection as a measurement**, and on a corpus with a real
   dictionary. Both caveats are in the probe's own output rather than only here, so a future run
   cannot quietly drop them.
4. **The staircase is worth having independently of this probe.** It is on `/control/status` now,
   and it is what turns "the fold used 9 GiB" into "the fold used 9 GiB in pass 3".
