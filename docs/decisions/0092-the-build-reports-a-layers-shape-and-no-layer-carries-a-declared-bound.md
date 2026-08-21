# 0092 — The build reports a layer's shape, and no layer carries a declared bound

**Date:** 2026-08-21 · **Status:** Settled (owner ruling) — **amended the same day** with the two
leak-register annotations the campaign's review raised ([the record](../evidence/memos/2026-08-21-artifact-serving-scale-review.md)).

## Context

[`artifact-serving-at-scale.md`](../design/artifact-serving-at-scale.md) §9 asked one question:
whether a layer whose membership has no row-space locality and no column to address it by carries a
**declared bound** on its artifact count — refused, warned about, or merely reported. It set out
three routes and recommended all three: **(a)** serve row-major wherever the layer partitions,
**(b)** a declared bound on what is left, as a warning rather than a refusal, and **(c)** report the
shape in the build's own report.

The question is older than that memo. Both [`artifact-delivery.md`](../artifact-delivery.md) §2 and
its Stage 6 discussion carried an owed item — *the per-request bound must refuse on the layer's
declared artifact count before any evaluation* — written when a predicate layer's only serving form
was one masked scan per artifact, so a nationwide boundary level was seconds per request and nothing
in the request could make it cheaper.

## The decision

**(c) always, (a) wherever the layer partitions, and no bound at all.**

- **Every build reports blocks per artifact**, per (layer, level), beside the reports
  [`configuration.md`](../design/configuration.md) §1 already writes. It is one number, and the
  build has already computed the row form it is counted from.
- **The row-major layouts are used wherever the layer partitions** — a label per row where the
  membership is single-valued, a list per row where it overlaps (§5).
- **No declared-bound machinery exists**: not a refusal at registration, not a per-request refusal,
  and **not a warning key on `[[layer]]`**. A layer that will be slow says so when it is built, and
  the operator decides whether to publish it.

This declines (b) in both of its forms. The memo's own recommendation is not taken in full.

## Why

**§5.1 removed the bound's motive.** A single-valued attribute predicate partitions the corpus, so
its natural storage is one label per row rather than one bitmap per artifact, and the cost becomes
points rather than artifacts — flat in the artifact count, and at 10⁹ points the only layout that
fits at all (4 GB against 78.5, derived from the residency campaign's measured 78.5 B per
container). What is left un-helped is a layer that is scattered **and** overlapping **and**
numerous: an enumerated set with no column. Every real instance of that shape is human-made or
vocabulary-made, the realistic counts are thousands, and at thousands the cost is 1.5–15 ms
(*measured*, §5).

**A bound on the artifact count bounds the wrong quantity.** What costs is row-space locality: a
scattered layer measures 96.8 row blocks per artifact against 1.0 for a clustered one, so 10⁷
clustered artifacts are cheaper to serve than 2×10⁵ scattered ones. A threshold on the count admits
the dear layer and refuses the cheap one, and no count the author declares says which they have.
⊘ *Both ends of that statistic are constructed by the probe's generators rather than observed, and no
measurement exists between 1.6 and 10 blocks per artifact — so the axis is measured and any threshold
on it is not.* Nothing in this ruling depends on the threshold: what is reported is the measured
number itself, and the operator reads it.

**A warning would have to be told the number the report already prints.** (b) compares a *declared*
count against a threshold; (c) states the *measured* shape of what was built. The layer a warning
exists for is exactly the one whose author mis-stated its shape.

**Nothing here leaks and nothing is irreversible.** Both layouts compute the same quantities from
inside `M_auth` — the principal's visible set — so **I2** is untouched, and **nothing on the wire
names a layout**; the probe asserts the served set identical ordinal for ordinal and, on the
row-major route, count for count. That is narrower than *the choice carries no disclosure content*,
which an earlier drafting of this paragraph claimed: the register carries two annotations covering
what the choice does put into the timing channel — a **C4**-shaped one for the candidate-generator
walk, whose service time varies with where artifacts the viewer cannot see sit in row space, and a
**C15**-shaped one for a layout flip being observable at a fold (`architecture.md` Appendix C,
owner-approved 2026-08-21). Both are Low and both are accepted. A wrong choice costs a rebuild. That
puts it outside the small enumerable surface where a refusal is the safe option, and inside the case
where the house rule is report loudly, print the numbers, and let the operator decide.

## What this supersedes

**The per-request bound is withdrawn, not deferred.** Three sites carried it and each is corrected
in the change that carries this decision:

- `artifact-delivery.md` Stage 6 — *"the per-request bound must refuse on the layer's declared
  count before any evaluation"*.
- `artifact-delivery.md` §2's second owed design item, *"due before Stage 6"*.
- `artifact-handover.md` §1.1, which restated it with the argument for refusing on a corpus-wide
  count rather than a per-principal one.

That argument was sound and is not what fails here: had a bound been kept, it would indeed have had
to read a quantity identical for every principal. What fails is the premise that a bound was needed
at all.

**Nothing about `artifact_budget` changes.** It stays what
[decision 0083](0083-the-frontier-is-a-request-time-budget.md) made it — a request-time control over
how much comes back, applied after the verdicts, bounding what is served and never what is
evaluated. It was never the cost control the withdrawn bound was reaching for, and this decision
does not make it one.

## Consequences

- No configuration key is added, no refusal is added, and no request field changes.
- The reported figure is per (layer, level), because a treed layer's levels have different
  populations and one number for the layer would be an average of two different problems.
- Where a scattered, overlapping, numerous layer does turn up slow, the answers are the layout it is
  given ([decision 0094](0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md))
  or the operator declining to publish it — not a bound added afterwards.

⊘ **None of it is built.** The build reports no blocks-per-artifact figure today, and every layer is
served artifact-major from cached row forms whatever its shape; the row-major layouts exist in
`crates/tessera-bench/src/bin/artifact_serving_scale.rs` as a measurement and not in the serving
path. Until they land, a scattered layer walls at ~2×10⁵ artifacts, and nothing warns about it — the
report is the first of the two to build for that reason.
