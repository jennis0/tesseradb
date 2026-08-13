# Recovering keyword `contains`: two fixes landed, three levers costed, one ruling owed

**Date:** 2026-08-13 · **Status:** Evidence memo — recommends, does not rule
**Measurements:** [`probes/2026-08-13-contains-recovery/`](../../../probes/2026-08-13-contains-recovery/)
**Reads with:** [the retirement fence](2026-08-13-utf8-retirement-fence.md),
[`records-and-search.md`](../../design/records-and-search.md) §4.3, §6.4

## The short answer

Yes, and both routes were paying for work they threw away.

**Landed, the broad route.** It walked the dictionary as `key.contains(needle)`, which constructs a
two-way searcher **once per key** and discards it. Hoisting the searcher recovers **1.14–2.03×** of
the whole walk (*measured*, three real columns, four needle lengths), and the walk is 99% of that
route. The change is `KeyMatcher` in `tessera-filter`, used by both routes; it adds no dependency
the crate did not already carry for the flat scan's region search, and the per-key test stays
unconditional, so the route's work is the same function of `(candidate, column)` it was before.

**Landed, the narrow route.** It called `key_of` once per candidate *entity*: entities sharing a
value probed the same ordinal repeatedly, and every probe decoded about half a restart block to
return one key. It now deduplicates the candidate's ordinals and hands them to a new
`SortedDict::walk_ordinals`, which decodes each block holding a wanted ordinal exactly once —
**1.91× to 6.05×, measured on all six shapes tried**, and never worse than the probe loop it
replaces, since a candidate with no duplicates whose ordinals share no block decodes exactly what
`key_of` decoded.

That second change also closes a reviewability gap rather than only a performance one. **Both
routes now end in the same `OrdinalPredicate::In` scan** and differ only in how the matching
ordinal set is found — the whole dictionary, or the blocks the candidate's own values occupy. The
narrow route was the one keyword shape whose traversal `take_scan_work` could not see, because it
added entities to the answer inside its probe loop; it is now asserted by the same harness as every
other shape, and a new test pins that the two routes traverse *identically*, not merely agree.

**And a correction to the fence, which is why the design's numbers need editing.** The fence's
broad arm was a reimplementation that *already* hoisted its searcher — its per-key figures match
this campaign's hoisted arm within 6% on four comparable cells and miss the shipped arm by
1.14–2.03×, and its own prose attributes the effect to `memmem`, which the shipped route never
called. So `records-and-search.md` §4.3's "1.5–71×" was never the tree's cost. Composing the
fence's per-cell totals with the measured walk delta (*modelled from measured parts*):

| | worst cell (`id`, contiguous) | best cell (`submitter`, scattered) | band |
|---|---|---|---|
| shipped when the fence ran | 144× | 2.5× | **2.5–144×** |
| shipped now, after the hoist | 82× | 1.8× | **1.8–82×** |
| + the ordinal bitset, unbuilt | 71× | 1.5× | 1.5–71× |

The recorded band is the third row — a route that is not built. §4.3 and §6.4 are corrected in this
change to state the shipped band and mark the third row as what the bitset reaches.

## The ceiling, and where the remaining gap now sits

**The broad route costs Ω(|dictionary|); the flat scan cost O(|candidate|).** No constant-factor
work closes that where the candidate is small, which is the per-keystroke cell and the one the
regression is worst in. Only the **narrow** route has the right shape there, and it is now 19.4–59.8
ns per candidate entity where it was 75–158, against the flat scan's 0.9–14.5.

**Which surfaces a miscalibration the change created.** The crossover prices the narrow route at
`NARROW_PROBE_NS = 100`, one `key_of` per candidate entity. That is still the correct *upper* bound
— a small scattered candidate over a unique vocabulary opens a block per entity and amortises
nothing — but it is now far above the typical cost, so the rule takes the broad route in cells where
the narrow one would win. Measured: on `id`'s contiguous 25% candidate the rule chooses broad at
41.1 ms where narrow now costs 11.6 ms, a **3.5×** loss to the route rule alone.

I have left the constant at 100 and documented why. Lowering it errs the other way, and the fence's
argument for which direction to err in still holds: the broad route's cost is capped by the
vocabulary, the narrow route's grows without limit in the candidate. Choosing well would mean a rule
that consults the candidate's *distinct* ordinal count — a statistic about what the principal's own
data contains — which is **the fence's stop-and-report A, still unruled**. It now has a price
attached, which is the useful thing this campaign adds to it.

## The remaining levers

Ordered by leverage against the cell each helps, with what each costs to build.

**1. An ordinal-set test that is constant per slot** *(the fence's own stop-and-report B, measured,
unactioned)*. The broad route's second stage hands `ValueColumn::scan_num_in` the 22,500 ordinals a
substring matched, where its "O(log k) per slot" is priced for the eight a caller types. A dense
bitset over the dictionary's ordinals answers in O(1) per slot: **63.88 → 38.86 ms** at 2.4M
(*measured, bench-local*). This is the step from the table's second row to its third. Contained —
one scan variant in `tessera-filter`, with the work-indistinguishability assertion the other scans
already carry.

**2. Parallelise the walk.** `decode_block` refuses a first entry with a non-zero shared prefix, so
every restart block decodes from empty and the blocks are **independent by construction** — the
buffer the walk threads across them exists for the cross-block order check, not for decoding.
Rayon is already the engine's compute pool. `÷ cores` is untested for this operator as for every
other, and §6.4 already carries `÷ cores` as this row's verdict; the point here is that the
structure permits it with no format change.

**3. A per-session needle cache, if the interactive cell is the one that matters.** The broad
route's matching-ordinal set is a function of `(dictionary, needle)` **alone** — it is computed
before the candidate is consulted, so it is mask-independent and principal-independent. And matches
are monotone in the needle: everything containing `abc` contains `ab`. A per-keystroke sequence can
therefore refine the previous hit set rather than re-walk the dictionary — on `id`, the fourth
keystroke would search 197,128 keys instead of 2,400,000, and if the cache holds the matched keys
rather than their ordinals, with no decode at all. Bounded by a per-session cap; the memory is a
few MB per live needle.

Kept **per session**, the only timing signal this creates is about the caller's own query history,
which is not a disclosure. **Shared across principals it would be a cross-principal timing channel
about what other principals have searched** — not about their data, but it is a channel and it
would need a leak-register row rather than an assumption. Recommend per-session.

## What needs a ruling, and is not proposed

**Needle-dependent pruning of the dictionary walk** — rejecting a key on its length before
materialising it, or a trigram bloom over the dictionary's keys — is the obvious next idea and it
is the one that changes the security shape. `SortedDict::walk`'s contract is that the broad route
reads every key whatever the needle is, which is what keeps its work a function of
`(candidate, column)` alone. Any prune makes the work a function of the needle's statistics against
the corpus's vocabulary, which is the class
[`2026-08-09-text-contains-acceleration`](2026-08-09-text-contains-acceleration.md) §2 flagged for
trigram postings as breaking §3.8 as a class, only weaker. **Not proposed here, and not to be added
as an optimisation without an owner ruling.**

Everything landed and everything proposed above is *unconditional*: each does the same work for the
same `(candidate, column)` whatever the needle contains. `walk_ordinals` stops at the last wanted
ordinal in each block, which is the same property — what it reads is fixed by the ordinals it was
handed, and those come from the candidate.

## Recommendation

Lever 1 is the last of the fence's own findings left unactioned and is contained — take it next, on
its own, since it is a change to the shared scan and wants the interleaved A/B discipline the
alignment memo describes. Treat 2 as part of whatever answers §6.4's `÷ cores` for the family as a
whole rather than for this operator alone. Hold 3 until there is a client typing into the surface,
and hold the pruning question until then too — it will look much more attractive at that point,
which is exactly when the ruling should already exist.

**The route rule is the one thing that would benefit from a ruling sooner.** Its cost is now
measured rather than hypothetical, and it is the difference between the narrow route's improvement
reaching a request and sitting behind a constant chosen for a route that no longer behaves that
way.

## A correction this memo owes about itself

The campaign's first four runs measured the narrow route's baseline with a **hoisted** searcher —
the fence's `narrow_contains`, not the route the engine shipped. That is the same defect this memo
reports in the fence, committed in the report of it, and it understated the recovery (1.63–5.03×
where the shipped comparison gives 1.91–6.05×). Two further corrections follow from it: the fence
harness is **not** unreadable — it is at
`7a24315:crates/tessera-bench/src/bin/utf8_retirement_fence.rs`, and reading it confirms both arms
hoisted rather than only the broad one this memo inferred; and the inference-from-figures method
this memo used, while it reached the right conclusion on the broad route, missed the narrow one.

Both were found by an adversarial review of this campaign rather than by the campaign. The general
lesson is the fence's, restated against itself: **a benchmark arm must call the shipped function or
say in the record that it does not.**

## What this does not measure

Nothing at 10⁹ and nothing out of cache: every dictionary here fits in this machine's L3 at 2.4M
keys, and the fence's §11 item 4 — the out-of-cache walk — is owed by this campaign in the same
terms. No arm re-times the ordinal scan, runs a parallel walk, or touches the write side.
