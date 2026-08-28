# Handover — the client's artifact cache, and the filter bit it needs

**Date:** 2026-08-28 · **Status:** **Steps 1–3 are done or ruled; step 4's corpus does not
exist.** The filter bit is ruled, written down and on the wire
([decision 0104](decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md), contracts r42,
`client-delivery.md` S13), and the channel now holds payloads across a served-set change (step
`cache 2`). Step 3 is designed, ruled and built
([`design/artifact-fetch-protocol.md`](design/artifact-fetch-protocol.md) r3, contracts r43): no
held-set claim — an opt-in column projection, the rung column, the two encodings, and a fetch model
that is policy rather than obligation, shipped in the channel. What remains is step 4 — a corpus to
measure on. This is a work list and the record of what was
established on the way to it, not a design.

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

- **`levels` on the request and `rung` on the *artifacts* frame** (0103, contracts r41; `level`
  renamed and re-meant at r43 — the drawing rung, computed server-side per layer kind).
- **`SessionArtifactTable`**, holding each artifact's `rung` straight off the wire — `rungOf` and
  the client-side `depth` are deleted, see §5.4.
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

**The wire affordance is designed and ruled** (2026-08-28): the client never says what it holds.
[`design/artifact-fetch-protocol.md`](design/artifact-fetch-protocol.md) §4 declines every shape in
which the client supplies rows — the held-set claim included — and its §5.2 specifies the one
affordance that survived, an opt-in column projection (built 2026-08-28, contracts r43).

**Whether a client may hold a level whole and pick from it locally.** The owner settled the
objection that this over-draws: an artifact's geometry and count are over its whole visible
membership, never the part in view, so an artifact drawn with its edge off screen never claimed
otherwise, and the server's intersection test is a fetch bound rather than an assertion. **Ruled
2026-08-28**: the fetch model is policy, not obligation — the obligations are the three rules of
the protocol document's §7.2.

## 4a. Step 3, costed — and why it should probably not be built yet

**Superseded by [`design/artifact-fetch-protocol.md`](design/artifact-fetch-protocol.md) r2**
(2026-08-28), which rules the space: the claim — (i) and (ii) below — is declined, (iii) is the
fetch model and is policy, and the one wire affordance is a column projection. ⊘ **The byte model
below is wrong, measured**: the 40 B floor is 97.1 B through the frame as built, so the elision the
claim buys is 22% and not 60% — the corrected table is the protocol document's §8. Kept for the
shape comparison; do not quote its numbers.

### 4a.1 The floor: the served identifier list, which no cache removes

A response's artifacts frame is two things — *which artifacts are in view* and *what each one is* —
and only the second is what the store now holds. So the affordance can elide payloads and cannot
elide identity, and the floor is what identity costs.

**Modelled, not measured**, from the frame as contracts §3.2 defines it: `tessera_id` 8 B,
`parent_id` 8 B, `level` 4 B, `matched` a bit, and `layer` as a **repeated string**, which is the
column that dominates — `clusters/toponymy` is 18 bytes on every row, there being no dictionary
encoding on this frame. That modelled 40 B; **measured through the frame as built it is 97.1 B**,
because a nulled fixed-width column still writes its slot, a nulled variable-length column still
writes its offsets, and `masked_count` cannot be nulled at all (the protocol document's §8 carries
the table and its provenance). **So payload elision alone saves 22% of the frame, not the 60% an
earlier revision claimed here**, and the scattered flat layer §1 is about — 231,645 artifacts,
served in full on every request — still pays ~22 MB a settled view with a perfect claim. A fifth of
a bad number is a bad number, which is half of why the claim was declined.

Dictionary-encoding `layer` is worth doing on its own and belongs to neither step: one column, no
semantics, ~14% of every full response (125.0 → ~107 B/row measured). The 13.6 B/row identity row
needs the projection's own schema (protocol §5.2), not an encoding change.

### 4a.2 Three shapes, and what each actually buys

**(i) A held-set claim by level** — *I hold layer L level k whole at content key X* — is the
handover's own proposal (§4). It is a few bytes, and the server elides the payload columns for that
level. **But it only elides the payload**, so it lands on §4a.1's floor; and a client can claim it
honestly only where it holds the level *whole*, which it knows only by having asked over the whole
extent. The claim's failure mode is benign — a false claim draws artifacts with no geometry, a
client bug that withholds nothing — but the win is bounded by the floor.

**(ii) A held-set claim by tile**, mirroring `delta-serving.md`'s declared-tiles form for points:
the client names the tiles it has already been answered for, and the server subtracts their
candidate set. Exact at any granularity and compact. **It is also the one that does not compose with
the rest of the request**: what a client was served for a tile depends on the `levels` it named and
on `artifact_budget`'s cut, so *served for tiles T* is not determined by T, and a claim that assumed
it would elide a payload the client never received. Making it sound means carrying the level set and
the budget in the claim and having the server reject any mismatch — a second request shape to keep
true against the first.

**(iii) No affordance at all: fetch the level whole, once, and pick from it locally.** 0103 already
expresses it — `levels: [k]` over the full extent — and the owner has already settled the objection
that local picking over-draws (§4: geometry and count are over the whole visible membership, so an
artifact drawn with its edge off screen never claimed otherwise, and the server's intersection test
is a fetch bound rather than an assertion). A client that holds a level whole then pans and zooms
within it with **no artifact traffic at all** — not a smaller frame, none — which is the only one of
the three that goes below §4a.1's floor.

### 4a.3 What (iii) costs, stated rather than implied

- **One large response up front** — the level whole for this principal, which for GeoNames' admin 4
  is 231,645 artifacts. 49 MB once is a different problem from 49 MB per settled view, and it is not
  obviously an acceptable one (the response-volume memo §5 Q2 says exactly this).
- **A filter still needs the server.** `matched` is per request (decision 0104), so a filtered view
  costs a round trip whatever is held — one that can ask for the bits alone.
- **Freshness is already answered**: the point path carries the content key on every response, so a
  client learns of a rotation without asking the artifact channel anything.
- **It is a client policy, not a wire feature**, so it needs no contracts change and no
  leak-register pass — the two things step 3 was expected to need.

### 4a.4 The recommendation

**Do not build a wire affordance yet.** Build (iii) as the channel's fetch model where the client
can hold a level whole, keep asking per viewport where it cannot, dictionary-encode `layer` to lower
the floor for the case that keeps asking, and leave (i) and (ii) unbuilt until something measures
them. The reason is §6 step 4's, unchanged: **the corpus that would show the difference does not
exist**, so any affordance built now is sized against a modelled number, and (i) is bounded by a
floor that (iii) does not have.

**Ruled 2026-08-28**: (iii) is the fetch model, and it is **policy rather than a written
obligation** — the obligations are the protocol document's §7.2, three rules. The client decides by
observation (S5 stays declined), and what it observes is the size of the answer it just got.

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

**5.4 `level` and `depth` are different numbers and only one is right per layer** — **moved
into the wire 2026-08-28** (the `rung` column, contracts r43; protocol §5.3): the server now serves
the drawing rung computed the right way per layer kind and `rungOf` is deleted, so no client picks.
The trap is kept as the record of why. The wire's `level` was the declared rung; `depth` the
parent-chain count. A **levelled** layer's resolution
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
3. ~~**Design the wire affordance** for *what I hold*.~~ **Done 2026-08-28**
   ([`design/artifact-fetch-protocol.md`](design/artifact-fetch-protocol.md) r2): there is no *what
   I hold* — every shape in which the client supplies rows is declined, and the affordance is an
   opt-in column projection (`artifact_rows`), specified with its schema and its disclosure
   reasoning — **built the same day** with the rung column and the two encodings (contracts r43),
   and the fetch model shipped in the channel, its idle promotion ungated by owner ruling.
4. **Measure on a scattered layer**, not on GeoNames. The corpus for it does not exist yet: an
   attribute layer over `admin4`'s 231 645 values on the GeoNames rung would produce one, and that
   is a corpus change rather than a code change.

**Steps 1–3 are done or ruled; step 4 is not.** ⊘ **Step 4's corpus does not exist.** Nothing in `test_corpora/` declares a scattered layer at
scale, so the case this work is for is currently unmeasured — say so rather than quoting the
regional figures.

## 7. Where status goes

**S13** is the row, added when step 1 landed — the convention that file already carries, and the
same one S10 used for the level. This document is the map; that table is the record, and it moves in
the change that moves the work. Steps 2 and 3 are client steps rather than server tracks, so they
join the steps table above it.
