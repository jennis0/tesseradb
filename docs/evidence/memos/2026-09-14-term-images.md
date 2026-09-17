# Row-space term images: the session projection built from what compresses

**Date:** 2026-09-14, evidence to 2026-09-16.
**Status:** Proposed. Not built. This is an evidence memo, not normative: it sets out the design
and the open choices from measurement; it does not decide them. Every figure below is measured,
modelled or assumed, and marked as such. The rung 6 figures are measured by
`probes/2026-09-14-projection-build/`, all three campaigns (2026-09-14 to 2026-09-16); its README
carries the full tables and is not yet on `main` (branch `probe/projection-build`). The open
questions are in §6.

## 1. The problem

A session's row projection is the image of its mask fragment under the view's permutation. It is
built by a walk, `Permutation::project` in `crates/tessera-store/src/permutation.rs`, which reads
one slot per set entity. Measured at rung 6 (3,495,729,729 rows, 24 GiB cap, permutation in the
page cache): 4.1 to 7.5 ns per entity, clustered terms near the low end and scattered terms near the
high end. A broad principal at 3,495,729,729 rows takes 10 to 23 s to walk and holds 288 to
437 MB afterwards, close to the 437 MB bitset ceiling a random subset of that size hits at the
entropy bound. Users hold between a handful and 100,000 terms, and each principal's access set is
near-unique, so caching whole session profiles does not remove this cost.

## 2. Term images

A **row image** is the row-space projection of one term's base postings, run-optimised, derived by
the same routine that builds a session's projection, at build and at compaction (decision 0139).
An image is kept only where it is dense enough to be cheaper to read than to walk: the ruled cut is
**more than 30 rows per container** (owner ruling, 2026-09-16), chosen by comparing 10, 30, 100 and
300 rows per container against the cost the image saves. At 30, measured at rung 6: `year` images
total 5.1 GB, `specieskey` images total 5.2 GB. The 254 country images total 11 MB at a cut of 10.

A dense or large clustered term pays for its image in disk and nothing else; a sparse term costs
disk and merge time for a saving the walk already gets cheaply, and is walked instead. A term
spread over the whole row space, such as `year`, is a dense bitset over most containers whatever
the term's own cardinality, so its image costs disk at every threshold measured: raising the cut
from 30 to 300 drops `year` from 97 to 35 kept terms but only 411 MB, because the bytes are in
bitset containers that survive every cut. `specieskey` behaves the opposite way: three-quarters of
its kept terms are sparse and drop between 10 and 30, for 14% of its bytes.

## 3. Three routes at session start

A session's projection can be built three ways once images exist.

- **Walk every held entity**, the route as built: 5.3 to 7.5 ns per entity measured on the
  principals above 45% access, 6.5 ns in the cost model.
- **Images plus residual**: union the kept images the session holds (about 350 ns per input
  container for array-heavy images, 870 to 1,120 ns for the bitset-heavy `year` images), then walk
  the terms without an image (about 11 ns per residual row).
- **Complement**: walk the entities the principal does not hold and subtract from the full row
  range, about 11 ns per entity outside the grant. This beats the plain walk only above about 60%
  access (modelled from the two rates: `share × 6.5 = (1 − share) × 11`), not at the 50% the parked
  complement branch (`store/complement-walk`) currently switches on.

None of the three is cheapest everywhere. Rung 6, five broad principals, measured 2026-09-16:

| principal | access | walk | images + residual | complement |
|---|---|---|---|---|
| species, weighted 1,000 | 49.1% | 12.9 s | 4.1 s | 16.1 s |
| countries, p50 | 50.0% | 9.3 s | 0.04 s | 9.9 s |
| year, uniform 300 | 64.6% | 16.3 s | 2.1 s | 12.1 s |
| species, weighted 10,000 | 78.6% | 17.5 s | 9.3 s | 9.6 s |
| species, weighted 100,000 | 89.8% | 19.8 s | 12.6 s | 4.6 s |

These times predate the fix to the walk's buckets, merged 2026-09-17
(`probes/2026-09-16-project-transient/`). It made the walk 15 to 29% faster on the same species
and country principals (weighted 1,000: 12.9 to 10.6 s; 10,000: 18.6 to 13.4 s; 100,000: 19.1 to
14.1 s; countries p50: 9.5 to 6.8 s, measured back to back). The residual and complement walks
run the same code, so they should gain too; that is inferred, not re-measured, and the table has
not been re-taken.

Access share alone does not choose: at 64.6% the images-plus-residual route wins by 5.5×, and at
89.8% the complement wins by 2.7×, and what differs between those two is input containers (2.1
million against 18.2 million) and residual rows (14.5 million against 547 million), not their
access share. A rule taking the cheapest of the three cost terms above, computed from term sizes
and the image table before any route runs, picks the fastest measured route for five of the six
principals above 45% access. It misses on the 10,000-term weighted species principal, where it picks
the complement at 9.6 s against the split's 9.3 s. This is modelled from eleven principals on one corpus, not measured as a rule.

## 4. Images mapped, not held

Images can be read by mapping the frozen file rather than by holding a built `Bitmap`. Anonymous
memory then holds only the view headers, 22 to 24 bytes per container, and the union's result. View
headers cost 50 to 406 MB for the four broad principals measured mapped, against 3.4 to 6.5 GB to hold the
same images built. The merge's peak is the same either way — the union's result, the projection before
`run_optimize` — because that peak is the output, not the inputs.

Mapped costs 5 to 20% more time than built for a warm union, above 1% access. A cold start, with
nothing in the page cache, reads up to 3.5 GB and takes up to 4.8 s creating the views; the first
union then reads up to 2.85 GB more. Images trade disk for time, and mapping keeps them out of
resident memory: the resident cost is the view headers and the result, however much of the image
file is touched.

## 5. The memory floor

A broad scattered principal holds about 430 MB after `run_optimize` on every route that holds a
projection, images or none. No route measured here changes that; it is the floor §1 already
states, at the entropy bound for a random subset of that size. Removing it is a sizing and
admission question, not a route choice, and this memo does not reopen it.

## 6. Open

**(a) Where do images live?** Not decided. Two options, both consistent with the measurements
above:

- In the bundle, written by build and compaction, the bundle format bumped to carry them. The
  bundle's digests cover them, which matters because an image with extra rows is a disclosure, not a
  slow session. Read cost is §4's; write cost is one derivation per dimension, measured at 2 to 3
  minutes each at rung 6, and compaction rewrites the images of the terms it touches. Changing the
  keep rule means rebuilding the bundle.
- In a derived cache beside the bundle, built lazily or on a schedule and independent of bundle
  format. Avoids a format bump and lets an operator skip the disk cost for a view nobody queries by
  term, at the cost of a miss path (walk, uncached) the bundle route does not have. It needs its own
  identity (bundle, permission plugin, segments version) and its own integrity check, since the
  bundle's digests do not cover it.

**(b) The walk's own anonymous peak: fixed.** The walk peaked at 2.4 to 4.6 GB of anonymous memory
for principals above 45% access, because each of its 834 row buckets kept the largest capacity it
ever reached. The buckets now share one chunk pool, and the peak is the result as held plus 68 to
72 MiB (measured, rung 6, merged 2026-09-17; `probes/2026-09-16-project-transient/`).

**(c) The route rule.** §3's cheapest-of-three rule is modelled from eleven principals on one
corpus. It has not been measured as a rule against a wider set of grants or another corpus.

**(d) No realistic user term sets.** No test corpus supplies a grant set shaped like a real
principal's; `specieskey` and `year` values stand in for user terms because they cover the sizes
and shapes a term dimension can take, not because a real deployment grants by species or year.

## 7. Ruled out

- **In-place merge treated as 50 ns per step.** Measured `fast_or` costs 144 to 1,740 ns per input
  container, 3 to 30 times the figure first assumed; the union's cost model in §3 uses the measured
  rate.
- **A keep rule at 10 rows per container.** The broadest species principal's split cost 24.7 s
  against a 23.2 s walk: below its own break-even. The rule is 30 (§2).
- **Access coverage as the session-start route rule.** §3 measured two principals at close access
  shares (64.6% and 89.8%) where the cheaper route differs and is not predicted by share; the route
  is chosen from term sizes and the image table, not from coverage.
