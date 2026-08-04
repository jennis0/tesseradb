# 0047 — Edit is delete + re-ingest, and a deleted entity is forgotten at the boundary

**Date:** 2026-08-04 · **Status:** Settled (owner ruling)

## The rulings

Two, made together, and the second is what makes the first workable:

**Edit is delete + re-ingest — the `predicate` op is withdrawn.** `/control/changes` refuses
`op: "predicate"` with a typed 422 naming the flow. Re-labelling an item is: delete it, then
re-ingest it under the same `external_id` with its new access labels. Content and geometry edits
were already this by construction (no update op exists); access edits now join them.

**A deleted entity is forgotten at the interchange boundary.** In the owner's words: *"once
they're deleted, they're gone"*, and *"us sustaining external IDs past deletion is an us
problem, not something we should block a user write on."* Concretely:

- **The ingest duplicate check counts only non-deleted holders.** A binding whose entity is
  deleted is dead bookkeeping, and it never refuses a user's write — enforced at both checks
  (the handler's naming 409 and the executor's apply-adjacent backstop, which reads the same
  generation its apply will clone from, so the exemption cannot race its own delete).
- **Re-ingest re-binds the external id.** The live map's insert overwrites; the sidecar walk
  runs **newest-run-first** (flush order is recorded; the build run is oldest), so after
  rotation reclaims the WAL rows the newest run's binding wins; and merge's run coalesce keeps
  the newest binding when a key appears in several inputs — repealing contracts §2.4's premise
  that a key cannot appear twice, which the duplicate check used to guarantee.
- **The forgotten holder accumulates nothing.** Changes addressed by the external id land on
  the live binding; the dead entity keeps only what it had — its deny entry, until the fold.

## The boundaries, stated because each is one slip from a hole

- **A *suppressed* holder still collides.** Suppression is temporary hiding, not deletion;
  re-ingesting a byte-identical copy past one is the copy-no-deny-can-reach hole the duplicate
  check exists to close. Deleted ⇒ forgotten; suppressed ⇒ still the holder.
- **Forgotten is not reused (I9).** The dead entity's ID stays burned forever; the re-ingested
  life takes a fresh one, and every mask or generating set holding the old ID stays valid.
- **Pre-fold masking is untouched.** The dead entity's row exists in its segment until the
  compaction fold; `deleted`, `denied[slice]` and the manifest `tombstones` go on hiding it
  exactly as before. "Forgotten" is an interchange rule, not a retirement.
- **`tessera_id` changes across an edit; `external_id` does not.** Consumers persist
  `external_id` by contract, so this is the trade the contract already made.

## What this dissolves

- **The novel-descriptor silent hide** (the invariants review's F4): a predicate change naming a
  descriptor the dictionary had never held minted an unsatisfiable extension id that nothing
  ever promoted — the item invisible to everyone behind a 200. With the op withdrawn, a re-label
  travels the ingest path, and flush promotion — the one promotion path there is — handles a
  novel descriptor exactly as it does for any new item.
- **The evaluate-entry fold obligations shrink to legacy.** New evaluate entries cannot arise;
  entries in pre-0047 WALs replay and compose as before, and the fold must still fold or refuse
  them (from their raw descriptors, never their resolved ids). **The machinery itself — the
  store, the WAL variant, `verdict`'s branch — is kept dormant, not deleted**: amputating it is
  a follow-on ruling once this one has lived, not a rider on it.

## Costs, stated

An edit burns an entity ID and re-buffers the item, so it is invisible for up to one flush tick
(inside §3's write budget); a client holding the old `tessera_id` gets the same 404 any deletion
produces. Sidecar resolution's worst case is unchanged in shape (every admitting run is still
searched; the walk direction is free); a coalesced run stays exactly as large as its live
bindings.

## Provenance

Owner, 2026-08-04, reviewing the write-path consolidation's novel-descriptor finding: package A
of the disposition, chosen over refusing novel descriptors on the predicate op (B) — "we should
just forget deleted entities… agree with A for now." Supersedes the 2026-08-03 deny-lifecycle
pass's bulk-addressing clause for predicate changes ("stay per-item"), which is moot with the op
withdrawn; every other clause of that pass stands.
