# 0094 — The serving layout is chosen at build, overridable per layer, and re-evaluated at every fold

**Date:** 2026-08-21 · **Status:** Settled (owner ruling)

## Context

One membership has several serving forms.
[`artifact-serving-at-scale.md`](../design/artifact-serving-at-scale.md) §5 measures two that are
genuinely alternatives — **artifact-major**, one row-space bitmap per artifact, which is what the
engine builds today; and **row-major**, one label per row where the membership partitions or one
list per row where it overlaps — and a spatial predicate has a third, its declared shape decomposed
to Morton ranges. Which is right is decided by row-space locality and by the artifact count, and the
two layouts cross where you would want them to: row-major is dearest at whole-map zoom, exactly
where the artifact-major extent test needs no scan at all.

Neither the locality nor the count is in the declaration. Blocks per artifact is a property of where
the data actually landed, and it moves as the corpus grows, as artifacts are published, and as a
fold retires them. So the layout cannot be a one-off decision taken from a config file, and it
cannot be left to the author either: the crossover table is a measurement, and it is not something
someone writing a layer block should have to hold.

## The decision

**Automatic, with a declared override, recorded per (layer, level), and re-evaluated at every
compaction fold.**

1. **The build or the registration picks the layout**, from what the layer declares and from the
   measured crossovers.
2. **The choice is recorded per (layer, level)**, beside the registered layer in the manifest.
3. **A per-layer configuration key pins it.** A pinned layer is served the layout it names, and
   combinations the layout cannot represent are refused at parse rather than ignored.
4. **The fold re-evaluates the automatic choice** from the shape it observes — blocks per artifact
   and artifact count, after the retirements the fold executes. **An override never flips.**
5. **A control verb that forces a layout post-build sets the override for the next fold**, rather
   than rewriting a live level. Deferred to the runtime-artifacts stage.

## Why

**The fold is the natural home for a flip.** It already rewrites every level's membership into the
new prefix and rebuilds the row forms, so a layout change rides a rewrite that is happening anyway.
Outside a fold, changing a layout means rewriting a whole level's membership for a latency gain,
while it is being served — which is a much larger operation than the thing it buys.

**The choice is recorded per (layer, level), not per layer, because the levels differ.** A treed
layer's coarse level holds ten thousand nodes and its leaf level ten million; a hierarchy's nodes
are contiguous ranges at every level while a scattered leaf population is not. One record for the
layer would average two different problems.

**The override is declared per layer, because that is where a declaration lives.** A file says what
a layer is, not what each of its levels should be stored as; a per-level override would be a
configuration surface for something the operator has no way to reason about level by level. The
asymmetry is deliberate: the pin applies to every level of the layer it names.

**An override a fold could overturn is not an override.** The whole point of pinning is a layer
whose measured shape says one thing and whose operator knows another — a level about to be grown, a
benchmark, a bug being cornered. If the next nightly fold silently reverted it, the key would be a
suggestion.

**A flip is not a client-visible event.** Both layouts answer identically — the probe asserts the
served set ordinal for ordinal and, on the row-major route, count for count — so nothing on the wire
names a layout and no client can tell which one served it.

## Consequences

- The layout is not a contract. It is a latency choice with no disclosure content
  ([decision 0092](0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)
  rules the same thing about reporting it), so it may change freely at a fold.
- Publishing artifacts into a level does not flip its layout, and neither does ingest. The shape
  that would justify a flip is observed at the fold, which is also the only place the flip is cheap.
- Whatever the fold flips, it must also drop — a level whose layout changed has projections in the
  old form that nothing will read again.
- The blocks-per-artifact figure the build reports under 0092 is one of this decision's own inputs,
  so an operator can see what the automatic choice was made from.

⊘ **None of it is built.** Today there is one layout — artifact-major row forms, rebuilt eagerly at
a generation move — with nothing recorded about it, no key to pin it, and a fold that rebuilds those
forms without re-evaluating anything. The surface this decision rules is designed in
[`2026-08-21-artifact-layout-selection.md`](../evidence/memos/2026-08-21-artifact-layout-selection.md),
which is input to the campaign's adversarial review and is not yet binding.
