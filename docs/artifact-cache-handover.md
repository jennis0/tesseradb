# Handover — the client's artifact cache, and the filter bit it needs

**Date:** 2026-08-28 · **Status:** **Steps 1 and 2 are done; the wire affordance is not.** The
filter bit is ruled, written down and on the wire
([decision 0104](decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md), contracts r42,
`client-delivery.md` S13), and the channel now holds payloads across a served-set change (step
`cache 2`). What remains is §6's steps 3 and 4 — the wire affordance, and a corpus to measure on.
This is a work list and the record of what was established on the way to it, not a design.

**Read [`client-delivery.md`](client-delivery.md) first** — it is the status record for client work,
on the convention [`artifact-delivery.md`](artifact-delivery.md) set, and it wins over this document
wherever they differ. **The row is S13**, which carries step 1; §7 says how the rest lands there.

## 0. Where authority lives

- [`design/annotation-representation.md`](design/annotation-representation.md) **§6** — serving,
  what bounds a response, and the cost model this work exists to make true. Its §6 says *"the client
  caches, so a level is replica sync rather than request cost"* and *"sizing a level as though every
  viewport re-fetched it is the wrong model"*. **That is a claim about a client that does not
  exist.**
- [`design/client-obligations.md`](design/client-obligations.md) — **rule 7** already states the
  obligation: *"a held whole-layer artifact set goes when the content key it was fetched under
  rotates, and on refresh"*. Rule 6 states that the artifact channel asks for itself and must never
  read artifacts off the point path.
- [`decisions/0103-a-request-naming-no-levels-is-answered-at-the-declared-ones.md`](decisions/0103-a-request-naming-no-levels-is-answered-at-the-declared-ones.md)
  — the level on the wire, merged 2026-08-28. Its *Consequences* carry the one rule this work must
  not break (§5.1 below), and its *What this does not decide* names this work as owed.
- [`design/artifact-serving-at-scale.md`](design/artifact-serving-at-scale.md) **§7** — the shape
  census. The three-row table there is why this work is not an optimisation; see §1.
- [`evidence/memos/2026-08-28-artifact-response-volume.md`](evidence/memos/2026-08-28-artifact-response-volume.md)
  **§4.4** — the two facts the cache turns on, with where each was verified.
- `contracts.md` **§3.2** — the *artifacts* frame and the `/v1/viewport` request as they now stand.

## 1. Why this is not an optimisation

The obvious reading is *the first request is already small since 0103, so a cache saves the second
one*. That is true for a **regional** layer and false for the case that has no other bound at all.

`artifact-serving-at-scale.md` §7 sorts layers by row-space locality, and the third row is the one
to design for:

| shape | what has it | blocks/artifact | in the tile index? |
|---|---|---:|---|
| clustered | HDBSCAN, point-and-radius, a hierarchy level | 1.0 | yes |
| regional | administrative boundary, spatial predicate | 1.0–1.6 | yes, via its extent |
| scattered | attribute predicate, per-analyst selection, terms-as-artifacts | 734–1 524 | **no — `everywhere` is 100% of the layer** |

A scattered artifact touches every node of the tile index, so it is never inside one and never
outside one: the walk returns it **on every request whatever the viewport**, measured at every size
in that campaign. And such layers are typically **flat**, so they declare no levels for 0103's
selector to bound and `artifact_budget` is inert on them by contract (§3.2). **Nothing bounds a
scattered flat layer's response** — not the bbox, not the level, not the budget — and the shape is
reachable from an ordinary declaration: an attribute layer over a 231 645-value column is 231 645
artifacts served in full on every request at every zoom.

The property that breaks every other bound is what makes the cache exact for that case: **the served
set is identical on every request**, so it is fetched once per session and never again. Where a
regional layer's cache accumulates what has been visited, a scattered layer's is complete after one
response.

**Do not size this work by the GeoNames win.** That corpus is regional and 0103 already bounds it.

## 2. What is established, and where each was verified

These were traced in the tree rather than argued, on 2026-08-28. Re-verify if the code has moved,
but do not re-derive them from scratch.

1. **An artifact's payload is a pure function of `(artifact, M_auth, generation)`.** Key, masked
   count, centroid, box, hull, content, parent and level — none depends on the viewport, the
   filters, `k` or the zoom. `tessera_engine::derived`'s module doc states it, and both call sites
   in `tessera_engine::viewport` compute over `mask.visible_rows(members)`: the artifact's **whole**
   membership intersected with the mask, never clipped to the requested tiles. **Only which
   identifiers intersect the view varies per request.** This is the whole basis of the work.
2. **The content key already means exactly *is what you hold still true*.** `view_coordinates` in
   `tessera_engine::viewport` hashes the identity key, the fragment watermark, the overlay version
   and the boot nonce — the same three things a payload depends on, and **nothing from the
   request**. So it rotates precisely when a held payload goes stale and never because a client
   panned, changed level or changed `k`. **No new coordinate is needed.**
3. **Half the client machinery exists.** `SessionArtifactTable` (`clients/ts/core/src/artifactTable.ts`)
   already names each artifact once per session, refcounts ordinals, holds geometry, and survives a
   view change — that is the payload store. What is missing is that `ArtifactChannel` replaces its
   served set wholesale on every response and re-fetches every payload with it.
4. **The wholesale replacement of the *served set* is correct and must stay.** `artifactChannel.ts`
   states why at the site: a merged set would show clusters for ground the user has panned away
   from. The work is to separate *which identifiers are in view* — replaced wholesale — from *the
   payloads I already hold* — accumulated. They are two structures, and the second one exists.
5. **No cardinality hint is available.** S5 was declined by the owner (2026-08-25): a layer's
   artifact count is a corpus-wide count over objects the principal may not individually see (C8),
   and a bucketed form still leaks an inequality about it. The fetch model is chosen by
   **observation**, which is what the store does today.
6. **A level is the unit to cache in**, and it only became expressible on 2026-08-28. For a
   geographic layer the move is to fetch a level whole once over the full extent — 254 artifacts at
   GeoNames' country rung, 4 842 at admin 1 — and then pan and zoom within that band with no
   artifact traffic at all.

## 3. What is already built and can be relied on

- **`levels` on the request and `level` on the *artifacts* frame** (0103, contracts r41).
- **`SessionArtifactTable`**, with `level` (the wire's declared level), `depth` (the parent-chain
  count) and `rungOf` picking between them — see §5.4.
- **Client-obligations rules 6 and 7**, which already say what a held artifact set is and when it
  goes.
- **The masked-count histogram cache** on the server side, keyed per level version with the overlay
  version as its deny edge. Server-side caching of *counts* is done. **⊘ Nothing caches derived
  geometry**, which `tessera_engine::derived::compute` recomputes per request over every visible
  member of every served artifact — a separate, purely internal saving this work does not need but
  sits beside, and one the level selection already reduced by serving fewer levels.

## 4. What is open

**The filter bit is ruled, written down and built** — [decision 0104](decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md),
2026-08-28, from the owner's words that filters should *"return boolean membership mask for filtered
artifacts"*. The shape, as it now stands on the wire:

- **A boolean per served artifact under a filter**: `membership ∩ M_sel ≠ ∅`, an early-exiting
  intersection, and **not a filtered count**. A count would put a second number beside the artifact
  and force the client to choose which it is showing, which is the ambiguity
  `annotation-representation.md` §6.3's ⊘ raises rather than an answer to it.
- **Existence and the masked count stay on `M_auth`**, so a filter still cannot make an artifact
  appear or vanish (**I3**, **I12**).
- **It is the only filter-dependent thing**, which is what lets a held set survive a filter change.
  Today client-obligations rule 7 drops held bands on a filter change because the identity key
  excludes filters — but the payloads do not go stale, only the bit moves.
- **The client cannot compute it.** The points it holds are a sample of `matched`, so deriving
  *which clusters still have members* from them gives false negatives for exactly the small clusters
  a filter is used to find — the **sample-as-set error in geometry**, stated at
  [`design/client-interaction.md`](design/client-interaction.md) §2. (`client-interaction.md` §2 pointed at
  `annotations.md` §9 for that rule, which is *What it costs*; it now points at §4.2, the closure
  rule a derived property obeys.)
- **No leak-register row.** Appendix C's preamble gives the inclusion test: *a row exists only where
  a viewer, reading responses they are entitled to, can end up knowing something about data they
  were not served*, and *data the service serves never qualifies*. The bit is over a subset of the
  principal's own visible members.

**One thing the ruling did not say, and the decision settles**: the bit is scoped to the request's
tiles rather than to the whole visible membership — `membership ∩ viewport ∩ M_auth ∩ M_sel`. Two of
the three routes a filter takes into row space are *silent* outside the request's own domain rather
than negative there, so a whole-membership bit would be exact on the projecting crossing and quietly
narrow on the per-tile and render-column ones. The cost is that the count beside the bit is not so
scoped, which the wire says at the field. 0104's *What is in view* has the argument.

**⊘ The wire affordance is undesigned.** How does a client say what it holds? A list of held
identifiers is worse than the payload it saves (464 655 ids is 3.7 MB). With 0103 landed, a level is
the compact unit — *I hold levels 0 and 1 of this layer at content key X* is a few bytes. That shape
is a proposal, not a decision.

**⊘ Whether a client may hold a level whole and pick from it locally.** The owner settled the
objection that this over-draws: an artifact's geometry and count are over its whole visible
membership, never the part in view, so an artifact drawn with its edge off screen never claimed
otherwise, and the server's intersection test is a fetch bound rather than an assertion. What is
undecided is whether the *fetch model* is written down as an obligation.

## 5. Traps — things that look right and are not

**5.1 The level must never enter the content key.** It is a request parameter; the content key
answers *is what you hold still true*, which a level change does not affect. Putting the requested
levels into it would rotate the key at every zoom band crossing and discard a cache that was still
valid. Recorded in 0103's *Consequences*; it is the kind of thing a later change "fixes" by mistake.

**5.2 Do not merge the served set.** See §2.4. The payload store accumulates; the served set does
not. Collapsing them draws clusters for ground the user has panned away from, and it will look like
the cache working.

**5.3 Do not read artifacts off the point path.** Client-obligations rule 6: the replica elides
tiles it already holds, and an elided tile contributes no artifacts, so a cluster's presence would
depend on whether its ground happened to be novel — **the map would lose clusters as the cache
warmed**. The channel asks for itself with `k = 0`.

**5.4 `level` and `depth` are different numbers and only one is right per layer.** The wire's
`level` is the declared rung; `depth` is the parent-chain count. A **levelled** layer's resolution
is its declared level, because an edge may skip a rung — counting links put 490 of
`clusters/toponymy`'s 797 artifacts at the wrong level. A **treed** layer declares no levels, so
every artifact arrives at level 0 and the chain depth *is* its resolution; reading the declared
level there collapses the layer to one rung, which removes the legend's level picker and makes the
coarsening walk a no-op. `rungOf` picks between them. This was shipped broken on the branch and
caught in review — do not re-collapse them.

**5.5 The filter bit is cheapest where it is least useful.** A scattered artifact's membership
spans 734–1 524 blocks, so the early exit helps when there **is** a hit and the common case under a
selective filter is a full pass. That is the cell to measure before promising a figure; it is
unmeasured today and must not be claimed.

**5.6 Two inertness rules, and they differ** (0103). A layer declaring **no levels at all** is inert
to a level selection in every form. A layer declaring levels but **no zoom range** serves every
level in the *absent* case only.

## 6. A suggested order

1. ~~**Write the filter-bit decision** (owner), then build it.~~ **Done 2026-08-28** (0104, S13):
   `matched` on the *artifacts* frame, null where the request carried no filter; the probe is
   `ArtifactRows::matched`, which asks candidacy's own three routes of a narrower set;
   `artifact_filter_bit.rs` proves the bit against the generator's closed forms and proves the
   served set and the masked counts unmoved.
2. ~~**Teach the channel to hold.**~~ **Done 2026-08-28** (`cache 2`): the served identifier list is
   still replaced wholesale and the payloads accumulate beside it in `ArtifactChannel`, keyed
   `(layer, tessera_id)`, dropped whole when the identity key or the content key the store was
   filled under rotates and on reset — rule 7 exactly. The ordinal reference is taken once when a
   payload enters and released when it leaves, so an artifact panned away from and back is not
   renamed; the store then reuses the colour map rather than rebuilding it, which is where the
   saving actually lands. The bit is read from the response every time and never from the store.
   **Without step 3 the response still carries every payload**, so nothing is saved on the wire or
   in parsing yet — what is saved is naming, colouring and the lookup texture. **No cap** (owner,
   2026-08-28): a cap counted in artifacts is not a bound in bytes, the two differing by orders of
   magnitude between a count-only layer and one carrying hulls, so the drop rules are the whole
   bound and a byte cap is left to be built if it is ever wanted.
3. **Design the wire affordance** for *what I hold*, once (1) and (2) have shown what the client
   actually needs. This is the step that needs a contracts change and a leak-register pass.
4. **Measure on a scattered layer**, not on GeoNames. The corpus for it does not exist yet: an
   attribute layer over `admin4`'s 231 645 values on the GeoNames rung would produce one, and that
   is a corpus change rather than a code change.

**Steps 1 and 2 are done; steps 3 and 4 are not.** ⊘ **Step 4's corpus does not exist.** Nothing in `test_corpora/` declares a scattered layer at
scale, so the case this work is for is currently unmeasured — say so rather than quoting the
regional figures.

## 7. Where status goes

**S13** is the row, added when step 1 landed — the convention that file already carries, and the
same one S10 used for the level. This document is the map; that table is the record, and it moves in
the change that moves the work. Steps 2 and 3 are client steps rather than server tracks, so they
join the steps table above it.
