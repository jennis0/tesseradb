# 0103 — A request naming no levels is answered at the declared ones, and there is no ceiling

**Date:** 2026-08-28 · **Status:** Settled (owner ruling, 2026-08-28).

## The decision

**A `/v1/viewport` request that names no `levels` is answered at the levels whose declared zoom
range covers the depth it asked at.** Naming `levels` — a list, or the string `"all"` — overrides
that entirely, and an empty list is *none*, as `layers: []` is.

**Two inertness rules, and they are different.** A layer that declares **no levels at all** — treed
or flat, sitting entirely at level 0 ([0082](0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md))
— is inert to this in every form: a level number names nothing about it, so a request naming levels
for the tiered layer beside it must not blank its clusterings. A layer that declares levels but **no
zoom range** on any of them serves every level in the *absent* case only, there being no map to
follow; `"all"` and an array still mean what they say there.

**The *artifacts* frame carries each artifact's `level`.**

**There is no artifact ceiling, and none will be built.** An over-large response is reported at the
build and served.

## Why

Three facts existed and nothing joined them: a layer declares a zoom range per level, `/v1/meta`
publishes them, and a request carries the same 0–16 depth coordinate. So the map that
[decision 0087](0087-cross-level-edges-are-information-not-rollup.md) named as a tiered layer's
resolution control — *"a coarser view is another level, chosen by the client"* — could not be acted
on: every level was served on every request. Measured on the GeoNames rung, an overview for a broad
principal carried **464,655 artifacts and 49 MB** where the declared map gives **5,096 and about
half a megabyte**, and the client discarded 88% of what it paid for. `configuration.md` asserted the
bound as fact while nothing could ask for it.

**The default is the declaration's, not *every level*, and that is the reverse of `layers`.** Naming
a layer has already opted into the artifact pass; what is left is which of its rungs to pay for, and
there the expensive answer is the one to ask for by name. A client that never thinks about levels
then gets the one a map would draw.

**The level column is a separate defect and lands with it.** A client had only `parent_id` to
reconstruct a resolution from, and that count is the depth of the chain that reached an artifact in
one response — a different question. A tiered layer's edges may skip a level and its roots may have
no parent to be given, so the two disagree on real data: 490 of `clusters/toponymy`'s 797 artifacts
were placed at the wrong level, 186 drawn at level 0 where 16 are declared. Walking parents stays
correct for a **treed** layer, where the lineage *is* the structure; the column is what stops that
reading being carried where it does not hold.

## Why no ceiling

The design leaned on one — `annotation-representation.md` §2 argued that optimising the fine-level
regime bought nothing because such a request *"will be refused on its artifact ceiling regardless"*,
and called its output unservable. No ceiling was ever built, and the owner declined to build one:
**degraded performance is better than no functionality.** A large artifact response is slow, not
wrong. It discloses nothing — it scales with the visible set and with the viewport and with nothing
else, which is **I2** behaving as specified — and a rerun costs nothing, so it is the recoverable
case the strictness rule hands to the operator rather than the disclosure case that earns a refusal.
Refusing here would also have been the register's own ratchet in behavioural form: a fail-closed
posture spent where nothing leaks makes the refusals that do protect the invariants stop standing
out.

What replaces it is a report. The build already prints artifacts per (layer, level); it now also
prints the layer's total and says that a response naming the layer carries the levels the request
did not exclude. **No byte estimate**, because bytes per artifact depend on what the layer declares
— a count-only level is tens of bytes and one declaring a hull is unbounded — and a constant would
be a guess wearing a measurement's clothes.

## Consequences

- **The zoom→level map stops being advisory** and becomes the default bound, overridable per
  request. 0087's ruling is unchanged in substance: the client still chooses a level. What changed
  is that following the published map is now the default rather than an intention with no
  expression, and ignoring it is the deliberate act.
- **`levels` applies to every layer named.** A level number is a rung of one layer and means nothing
  across two, so there is no per-layer map on the wire; under
  [0096](0096-layers-are-usually-one-and-the-picker-offers-the-closure.md) a request names one layer
  anyway, and the default needs no map because each layer's own ranges decide for it.
- **A level a layer does not hold is absent, not a refusal** — the route an unreachable layer name
  takes, and for the same reason: asking is not a way to learn what exists. Any spelling that is
  neither a list nor `"all"` is a `422`, as `layers` is.
- **A level selection cannot orphan a dependent.** A dependent whose target was not served is absent
  entire, whatever withheld the target, so a level filter is not a hole in that rule — the names of
  a boundary set never outlive the boundaries.
- **No disclosure change, and no leak-register row.** Every artifact a level holds passed its own
  existence criterion against `M_auth` before any of this ran
  ([0080](0080-the-frontier-is-a-per-artifact-test.md)), so asking for fewer levels serves strictly
  less and asking for more serves only artifacts that had already cleared their own test. The
  `level` value is the declaration's, identical for every principal, and already published in
  `/v1/meta`. Under Appendix C's inclusion test — *a row exists only where a viewer can end up
  knowing something about data they were not served* — this is data the service serves, which never
  qualifies.
- **The level must never enter the content key.** An artifact's payload is a function of
  `(artifact, M_auth, generation)` and of nothing in the request, and the content key hashes exactly
  those; so it answers *is what you hold still true*, which a level change does not affect. Putting
  the requested levels into it would rotate the key on every zoom band crossing and discard a cache
  that was still valid.

## What this does not decide

The **filter axis** stays open (`annotation-representation.md` §6.3's ⊘): filters still do not touch
artifact existence or counts. A **client-side artifact cache** and an **authored per-artifact rank**
are both consequences worth taking and neither is taken here.

