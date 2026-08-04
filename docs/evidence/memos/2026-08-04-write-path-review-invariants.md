# Write-path review — invariants and fail-open lens

**Status:** Evidence — review transcript, never normative. One of three independent reviews of
`docs/design/write-path.md` (then r2); companions:
[fidelity](2026-08-04-write-path-review-fidelity.md),
[memory](2026-08-04-write-path-review-memory.md). **Dispositioned the same day** (write-path r3),
with two findings then **ruled and closed the day after they were recorded as proposals**:
F3's tick-driven rotation and F6's step-down gates are built (write-path r4), and F4's
novel-descriptor hide is **dissolved** by decision 0047 (the predicate op is withdrawn; a
re-label travels the ingest path, whose flush promotion handles novel descriptors). F1 and F5
stand as recorded obligations on unbuilt machinery. The attacks-that-failed section is kept in
full so they are not re-run.

## Findings

**F1 — Rule F's identity match is not upheld by the only publication seam; the two named gaps
were not the whole list, and the third survives to disk (fail-open, latent behind unbuilt
compaction — the biggest).** `FragmentCache::bundle_identity` is the `CURRENT` manifest digest
read once at `Engine::open`, immutable for the process, and it keys both the in-memory slots and
the **persisted** `.frag` files. The one geometry-publication seam (`Engine::publish_geometry`)
carries no postings reader and no identity — its own doc calls it "compaction-shaped" because it
presumes the term index and dictionary are unchanged, the exact premise the fold breaks.
Scenario: delete E → fold executes it and (Rule F) drops it from `deleted` → nothing rotated the
fragment identity → a cold session's `get_or_build` recomputes the same key, hits the same
persisted fragment still containing E, and E's row is no longer in `denied[slice]`: **the
deleted item is served**, across restarts of the fragment map. Violates I2 and I1. Recorded as
the fold's first obligation (write-path §5.4's third gap, §8): the fold's publication path must
carry new postings and rotate the fragment identity — or the fold is an offline operation
(publish, then restart), and the spec must say which.

**F2 — "disk-full never refuses a deny" described absent machinery** (same substance as the
fidelity lens's F2; the terminal state on a full device is a torn WAL and 500s until restart,
which is what the config error message claims the relation exists to prevent). *(Owner: runtime
ceiling is a nice-to-have; document states the truth.)*

**F3 — rotation had exactly one call site, inside `publish_flush`: a deny-only node never
rotated, never checkpointed, and its WAL grew without bound** (availability; terminal state is
F2's torn WAL, or an unbounded full-log replay). The lane structurally cannot be shed, nothing
measured the log, and "steady-state retention is two members" was a claim about a flushing node
only. *(Ruled and built: growth-gated tick rotation.)*

**F4 — a predicate change naming a descriptor the dictionary has never held hid the item from
every principal, permanently, behind a 200.** `commit_denies` minted an extension id;
`satisfied` can never contain one (authorise resolves by dictionary lookup only); promotion's
novel set is built from buffered ingest rows, and an evaluate entry is not one — so nothing ever
promoted it, and rotation re-minted the id. Plus an unlisted fold obligation: §4.3's "no
extension id ever reaches a durable file" was stated for the flush and not carried to the fold,
which would otherwise bake a process-local id into base postings and destroy the raw descriptor.
*(Dissolved by decision 0047; the fold obligation is recorded, legacy-scoped.)*

**F5 — 0044's stale-serve is sound as argued, but the projection and fragment are keyed
independently, so "one refresh later" is not a bound the keys can hold.** The projection key
carries no fragment identity; today the coupling is a request-ordering property
(`fragment_for` runs first, always at the live watermark), and stale-serve deliberately breaks
that ordering — a projection derived from a stale fragment would be inserted at the *new*
`segments_version` key and pin the session's freshly flushed items invisible until the next
publication (fail-closed; silently falsifies the ack→visibility bound). Second: "merge racers
are shed 429" is understated — a racer landing between the swap and the refresh's claim takes
the inline path and pays the measured 10.7 s rebuild, so the refresh must claim resident keys
**before** the swap. *(Both recorded as the 0044 mechanism's obligations, write-path §4.6.)*

**F6 — §2.4's step-down 503 was not implemented and nothing on the write path was gated on
step-down** (acked-ingest loss: a stepped-down writer that flushed assembles its manifest from
the older served state at a higher `n`, permanently shadowing the stepped-past segment).
*(Ruled and built: ingest, plan and rotation gates; denies exempt.)*

**F7 — §2.1's duplicate-check claim was unscoped**: the guard is conditional on the caller
supplying external ids — a row without one establishes nothing the check can see, and a fresh
batch id over identical bytes is not caught by idempotency. A deployment that suppresses must
ingest with external ids. *(Scoped in the document; decision 0047 then re-scoped the deleted
half.)*

**F8 — §5.8/§11 omitted the deny lane's label consequence**: a deletion invalidates every label
whose generating set held it, for every principal (I8's availability half), with the §2.5
notification obligation — the deny lane is the triggering event and had no row. *(Added.)*

## Attacks that failed (recorded so they are not re-run)

- **0044 stale-serve → deny miss**: four constructed interleavings (flush during a stale window;
  unsuppress of flushed-while-suppressed; predicate grant/revoke on a new-extent entity;
  suppress during refresh) all fail closed. Verified reasons: `derive_denied` runs at every
  geometry publication with the equality debug-asserted at the single publish site;
  `RowSpace::with_extent` refuses any extent whose `row_base` is not the current row total, so
  an append never moves an existing row id; `compose` computes diffs from the **live** `row_of`
  and applies `andnot(denied)` last and unconditionally. A stale projection is a subset of the
  fresh one — the fail-closed direction.
- **Merge racer served something wrong**: no — `extends_to` refuses whenever the extent list
  shortened, and the test-only `from_rows` is the only producer of the always-extends arm. The
  racer is served *late*, not wrong (F5's second half).
- **Row-projection patch across a prefix change**: closed by the key's `prefix` field.
- **Apply-anyway → side-manifest**: no ordering found — every failure route sets a WAL poison,
  both publication sites check it, and the only exit from a recoverable poison sets
  `overlay_diverged`, the second gate. The dirty-flag rule is genuinely belt-and-braces.
- **Unsuppress applied without durability**: no — the fold filters `Delete | Suppress`, and
  `apply_changes` derives re-derivation need from the *applied* list.
- **Seed-before-replay / snapshot resurrection, single partition**: not reachable — rotation
  runs only after a manifest reflecting the live overlay is durable at a higher `n`.
- **…except across partitions** (⊘ none exists): `write_deny_state` writes the global deny sets
  into whichever partition manifest is written, a flush refreshes only its own partition, and
  open **unions** every partition's seed — two partitions could rotate away an unsuppress that a
  sibling manifest still carries, and the union re-seeds it. A premise violation for the
  sharding stage, recorded at write-path §6.
- **§11 mapping errors**: none beyond the F8 omission.
