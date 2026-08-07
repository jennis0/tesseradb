# 0050 — A fold invalidates the term index and every mask fragment

**Date:** 2026-08-06 · **Status:** Settled (owner ruling)

## Context

`architecture.md` §11.3 said, of a compaction:

> At single-node scale it does **not** invalidate the term index, masks or generating sets.

`compaction.md`'s fold negates that twice. Pass 2 subtracts the tombstone bitmap from every term,
so the base postings are rewritten. And the flip publishes a new prefix, rotating the
`bundle_identity` every cached fragment is keyed by, so no pre-fold fragment survives.

The contradiction was found by `compaction.md`'s r3 adversarial round, which could not resolve it:
`architecture.md` is the specification and every mechanism document defers to it. It is also a
contradiction the section already had with **itself** — two paragraphs below, r33's deletion clause
reads *"at the next compaction drop its row **and fold its postings out of the term index**"*.

## The decision

**§11.3's sentence is wrong and is corrected** (architecture r34). A compaction rewrites the term
index and invalidates every mask fragment. Generating sets are untouched.

## Why

**Rule F, and it is not a matter of taste.** A deletion retires only at the fold that executes it.
If the fold dropped the entity's row and left its postings standing, retirement would withdraw the
overlay entry — the only thing hiding the item — while the entity remained in the term index. The
item would then be drawn, counted and served to every authorised principal, permanently. So the
two halves are not separable: **both, or neither**, and "neither" means compaction cannot discharge
the first of its three obligations at all.

The denial in the old sentence was true of a *rows-only* compaction, which is not what this system
has: `compaction.md` D5 ruled the row-space fold primary, and even the rows-frozen variant recorded
at its §13 rewrites postings — that mode's entire enabling property is that removing an entity's
postings is what makes it invisible.

## What it costs

The publication seam. `publish_geometry` is *compaction-shaped* only while the new prefix's term
index and dictionary are the same ones, which is exactly the premise this breaks;
`compaction.md` §4 enumerates the four gaps that must close, of which moving `bundle_identity` and
its `FragmentCache` onto the generation is the largest. That cost is now the specification's,
recorded here rather than absorbed quietly by a mechanism document.

## What was considered and declined

- **Hold §11.3 and forbid the fold from touching postings.** Declined: Rule F then retires nothing,
  the overlay grows monotonically under deletion churn for ever, and `overlay_soft_limit`'s alarm
  keeps having no lever. Compaction would be reclamation and reorganisation only — two of three
  obligations, and not the invariant-bearing one.
- **Leave both sentences standing and let `compaction.md` note the tension.** Declined: precedence
  means the specification wins, so an implementation written to the mechanism document would be
  the one in the wrong. A contradiction inside the corpus is not resolved by writing it down twice.
