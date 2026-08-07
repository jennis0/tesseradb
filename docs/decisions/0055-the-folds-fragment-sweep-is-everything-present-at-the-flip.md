# 0055 — The fold's fragment sweep is everything present at the flip

**Date:** 2026-08-06 · **Status:** Settled (owner ruling)

## Context

`compaction.md` §8 asks a fold to sweep the persisted fragment-cache directory "of entries under
superseded identities (nothing else will ever name them)". A fold rotates the bundle identity
(decision 0050), so every `.frag`/`.meta` pair written under the old one becomes unreachable: the
cache key hashes the identity, so no post-fold lookup can ever compute those names again. Left
alone they are a pure leak, growing by one wide-grant fragment per credential per fold.

**The set §8 names is not selectable the way the sentence implies.** An entry is
`<canonical key>.frag`, and the canonical key is
`SHA-256(bundle_identity ‖ auth_plugin_hash ‖ sorted term ids ‖ watermark)`. A hash does not
invert, so a filename cannot be traced back to the identity that produced it — and nothing beside
the file carries one either: the `.meta` sidecar holds a watermark, a length and a digest.

## The decision

**Sweep everything present, and take the listing before the swap while deleting after it.**

At the instant the identity rotates, every existing entry is under the superseded one. So
"everything present now" *is* the set §8 names — exactly, not approximately — and no inversion is
needed.

## Why the ordering is two steps rather than one

Either half alone introduces a hazard, and they are different hazards:

- **Delete before the swap** and a publication that then fails has discarded the cache that is
  still the live one. Correctness-neutral (the cache is derived and rebuilds), but it charges every
  resident credential a rebuild for a fold that did not happen.
- **Delete after the swap, by re-listing**, and the listing races a request that authorised in the
  interval and wrote a *new* entry under the *new* identity — which the sweep would then delete,
  again costing a needless rebuild.

A listing taken **before** the swap names only superseded entries and can never name a later one.
Deleting that list **after** the swap therefore closes both: nothing written post-swap is in it,
and a publication that fails deletes nothing. It also needs no mtime heuristic, which was the other
way to filter and is fragile against clock and filesystem behaviour.

## Why unlinking is safe while requests hold fragments

A `FrozenFragment` is a mapping, and on POSIX a mapping outlives its directory entry: a `Session`
holding one, or an in-memory `FragmentCache` slot, keeps reading the same bytes after the unlink.
What the unlink removes is the *name*, and nothing will compute that name again. The in-memory tier
is not swept at all — `FragmentCache::rotate` already starts the new cache empty, which is what
makes the pre-fold entries unreachable in memory (decision 0050's obligation, closed at the seam).

## What was considered and declined

- **Put the identity in the filename** — e.g. `<identity prefix>/<key>.frag` — so the superseded
  set becomes a directory listing. Declined: it is a change to the on-disc cache layout, and its
  only gain is the ability to sweep at a time other than the flip. Nothing wants that. A cache an
  upgraded binary cannot read is reclaimed by deleting the directory (`FragmentCache`'s own
  no-format-version ruling, 2026-08-02), so the layout has deliberately stayed nameless.
- **Sweep lazily in the background, by age.** Declined: it needs the identity to be recoverable or
  a clock to be trusted, and it buys nothing over one pass at a moment the fold already owns.
- **Do not sweep.** Defensible — the entries are unreachable, so this is a leak and never a
  disclosure. Declined because the leak is unbounded in fold count and the sweep is one directory
  listing on a path that already rewrites the corpus.

## What it obliges

- Failures are counted, never propagated: an unreadable cache directory yields an empty listing and
  a failed unlink is skipped. This is reclamation of derived data, and **a fold must not fail
  because a cache directory was unreadable**.
- The sweep runs inside the geometry publication, so it fires for a `PrefixRotation` and for
  nothing else. A flush or a merge rotates no identity and must not sweep.
- §8's sentence is corrected at the claim rather than left to be re-derived: the set is not
  selectable by name, and does not need to be.
