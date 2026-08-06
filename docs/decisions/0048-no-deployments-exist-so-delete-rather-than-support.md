# 0048 — No deployments exist, so delete rather than support

**Date:** 2026-08-05 · **Status:** Settled (owner ruling)

## The ruling

In the owner's words: *"Tessera isn't yet live. Outside of this repo, no deployments exist. We
should delete any code we don't need and don't worry about supporting existing users."*

Machinery whose only justification is a state some earlier version could have produced is
**deleted, not carried**. There is no bundle, no WAL and no client outside this repository, so
"a reader might still hold one" is not a live premise and must not be used as one.

This joins the reasons already recorded for the same posture in narrower form: `bundle_format = 1`
has never been published (contracts §0.3 deviation 5), which is why cutting the `priority` column
was free (decision 0046), and `api_version = 1` has no published reader, which is what keeps the
slices design's ingest map cheap (roadmap).

## What it licenses now

**The `evaluate` machinery goes.** Decision 0047 withdrew the `predicate` op; every remaining
piece of the evaluate path was retained for one reason — *"entries arise only from pre-0047
WALs"* (write-path §5.3) — and there are none. So: the `evaluate` store and `PredicateChange`;
the WAL's `Change` variant (already written by nothing) and the predicate arm of
`ChangeByEntity`, at a `WAL_VERSION` bump; the deny window's deferred descriptor resolution
(write-path §5.2), whose only consumer was an evaluate entry; the evaluate arm of `verdict` and
of composition; and the fold's evaluate pass, which dissolves compaction §12's D4 rather than
answering it.

Two things survive the deletion and one of them is the point of it:

- **The overlay stays two independent stores**, `deleted` and `suppressed`, of two different
  types. It does not become one map with a disposition field. Three stores existed because
  three facts retire three ways and a last-write-wins collapse was caught fail-open in review
  twice (architecture §11.2's marker, write-path §5.3); two facts retiring two ways — Rule F and
  Rule S — keep exactly that argument. Deleting a store is not collapsing the remaining ones.
- **`deleted > suppressed > buffered`** stays single-sourced in one function.

Architecture §11.2's *evaluate* disposition is the specification's, not a leftover, and this
ruling does not amend it: what it says is that one overlay covers both a predicate change and an
administrative suppression. What is deleted is the implementation of an op that no longer exists.
A future predicate mechanism would be designed, not resurrected.

## What it does not license

The distinction is between **compatibility with a past** — which does not exist — and
**defences for the present**, which do.

- **Fail-closed guards stay.** The WAL's positional recovery, the sidecar guards, honour-before-
  verify, the apply-anyway fold's asymmetry, seed-before-replay: none of these protects a legacy
  reader. They protect this node's own data across its own crash.
- **Contracts with a second reader stay.** The Python oracle, the conformance suite and a future
  engine version reading today's bundle are the readers contracts §0.1 exists for. "No
  deployments" removes the *third* reader, not the first two.
- **Format stability rules stay.** `seg_id` never reused, dictionary extents positional,
  `SEGMENTS-<n>` monotone: these are invariants of a *running* process, not of an upgrade path.
- **⊘ markers are not resolved by this.** Machinery that is specified and unbuilt stays marked;
  this ruling is about machinery that is built and unneeded.

## Consequences

The deletion sweep touches write-path §5.2/§5.3/§5.4/§5.8, architecture §11.2's parenthetical,
contracts §3.4's op list, conformance §5's script 5 (already void twice over), and code in
`tessera-engine`'s write and compose modules and `tessera-lifecycle`'s WAL. It is **its own
change**, not a rider on compaction: compaction only needs the fold's evaluate pass never to be
written.
