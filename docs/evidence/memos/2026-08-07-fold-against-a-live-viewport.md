# What a fold costs a live viewport — probe P4

**Date:** 2026-08-07 · **Status:** Evidence, never normative
**Subject:** `compaction.md` §6.1's `MADV_SEQUENTIAL` mitigation (decision 0052), which §14 has
carried as **unmeasured**, and — as the thing that has to be measured first — what a **real fold**
costs a concurrent viewport.
**Harness:** `crates/tessera-engine/tests/scale.rs`,
`a_live_viewport_against_a_real_fold_with_and_without_the_advice`, `#[ignore]`d. Raw output:
`probes/2026-08-07-fold-vs-viewport/`.

---

## Results

**A fold costs a concurrent viewport 1.05–1.18× at the deepest zoom, and the mitigation's effect is
below this probe's noise.**

| | measured | |
|---|---|---|
| a real fold's cost to a concurrent viewport, deepest zoom | **1.05–1.18×** | four runs, evicting regime |
| the same at shallower zooms | **0.96–1.12×** | i.e. at or inside the drift |
| `MADV_SEQUENTIAL`'s effect on that | **not resolved** — the two pairs disagree about sign | |
| its effect on fold wall clock | **not resolved** — `on` faster both times, by 41% and by 8% | |
| fold wall clock at one configuration, run to run | **varied 4.7×** (561 s to 2656 s) | |

**The headline is the first row, not the mitigation.** P3 measured a buffered reader streaming the
bundle flat out and found up to **2.03×**; a real fold, in the same kind of regime, costs about a
fifth of that. A fold is not a `dd`: it interleaves five passes, writes as much as it reads, spends
real time in Roaring and Arrow, and its own output competes with its input for the cache. P3's
figure is an upper bound on the harm and was always described as one — this is the harm.

## A correction, stated because it was nearly a ruling

**A first pair of runs appeared to show `MADV_SEQUENTIAL` making the fold 1.69× faster and the
viewport 1.46× worse**, which reads as the hint buying speed the design does not want (§6.1: *"a
slower fold is an acceptable price for a gentler one"*) and paying for it in the one thing a viewer
observes. That is not what the evidence says.

That pair ran while a full `cargo test --workspace` was running on the same machine. The reversed
pair, on an idle machine, puts `on` *ahead* at the deepest zoom (1.05× against 1.18×) and cuts the
duration difference from 41% to 8%. **Neither pair is strong enough to rule on, and the first is
contaminated.** It is in `probes/` labelled as such rather than dropped, because discarding a run
quietly is how a campaign comes to quote only the numbers it liked.

## What the probe is, and why it is not P3 re-run

§6.1 said P3 must be re-run with the hint applied. It cannot be: P3 models the *harm* with a
buffered reader, and the hint lives on `SegmentCursor` and `RunCursor`, which only a real fold or
merge constructs. There was no fold when P3 was written. P4 runs one — passes 1 and 3 over their own
mappings, pass 2 over the live readers — with a session sweeping [`ZOOM_SWEEP`] throughout, and
takes each arm in its own process over its own copy of one prebuilt bundle, because the arms differ
in what the page cache holds and one process would hand the second arm a cache the first warmed.

## What would settle the mitigation, and what would not

**Not more of this.** The differences at issue are 12–26% and the `quiet-after` drift within a
single run reached 23%; a fifth pair on this machine buys another sample of the same variance. What
is needed is either

- **repetition at a scale where the effect exceeds the noise** — n ≥ 5 per arm, interleaved rather
  than blocked so device drift cannot align with an arm, on a device nothing else touches; or
- **a bundle:cache ratio a deployment would actually have.** Both P3's real runs sat at 1.24:1 and
  1.96:1 and this one at 1.27:1, where a 47 GB bundle on a 16 GB machine is ~3:1. The hint's whole
  subject is which pages get evicted, and at a ratio near 1 there is not much eviction to steer.

**The variance itself is worth naming.** Fold wall clock ranged 561 s to 2656 s at one
configuration on one machine. Any future campaign that quotes a fold's duration from a single run
is quoting the machine's mood.

## Recommendations

1. **Keep the hint** (decision 0052 stands). It does no measured harm, the clean pair puts it
   slightly ahead on every axis, and it is one call per mapping. What changes is §14: its effect
   goes from *unmeasured* to **measured and not resolved**, which is a different and more useful
   claim.
2. **Record the fold's actual viewport cost as measured** — 1.05–1.18× at the deepest zoom — and
   stop citing P3's 2.03× as what a fold does. It is what an unthrottled streaming *reader* does,
   which is the upper bound and not the operation.
3. **Do not escalate to `MADV_COLD` on this evidence.** §6.1 names it as the next step if the hint
   proves insufficient; nothing here shows the hint is insufficient, because nothing here shows a
   viewport is suffering much in the first place.
4. **Before quoting any of this at 10⁹**: the regime here is a 3 GiB cgroup around a 3.81 GiB
   bundle, and the cgroup's hard limit is what produced P3's discounted 15.7× excursion. Use it to
   compare arms; use a real larger-than-RAM bundle to quote an absolute.
