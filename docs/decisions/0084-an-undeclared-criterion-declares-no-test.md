# 0084 — An undeclared criterion declares no test, on every route

**Date:** 2026-08-16 · **Status:** Settled (owner ruling)

## What this answers

Stage 2 shipped both routes an artifact can be reached by, and they disagree in one configuration.
A layer declaring `visible_when = null`, and a principal whose masked count for some artifact is
**zero**:

- **The viewport withholds it.** The candidacy test is *does any member visible to this principal
  fall inside the requested tiles*, and an artifact with no visible member fails it in every
  viewport, at every zoom, for ever.
- **The drill-down serves it**, with `masked_count: 0`. The identifier route applies the existence
  predicate alone, and with no criterion declared there is nothing for a zero count to fail.

Identifiers are stable across principals ([C17](../design/architecture.md)), so a principal handed
an identifier out of band learns that the artifact exists and that they can see none of it.

## The decision

**The declaration governs. `visible_when = null` means no existence test, and the drill-down's
answer is correct.** An operator who declares no threshold has declared no threshold, and the
service does not add one — a floor the schema cannot express, applied on the service's own
initiative, would be a rule nobody wrote and nobody could turn off.

**No code changes.** The behaviour above is already what ships; what changes is that it is intended
rather than open.

## What was declined, and why the argument did not carry

The alternative was an implicit floor: **serve nothing at a zero masked count unless the layer
declares `artifacts_carry_own`**, on the reasoning that with no visible member nothing inside
`M_auth` witnesses the artifact, so reporting its existence is information from outside `M_auth`
and therefore I2's business.

Declined on the ground that it invents a rule the declaration surface does not contain. The
criterion is exactly the control for this, `min_visible = 1` expresses the floor precisely, and a
service that silently applied one would make two layers with identical declarations behave
differently for reasons a reader of the declaration could not recover. **Where a deployment wants
the floor, it declares it** — and that is a one-word edit, discoverable from `/v1/meta`, and
auditable.

I2 is not weakened by this. The *count* is still computed from inside `M_auth` alone and is still
exact; what an undeclared criterion permits is the disclosure of an artifact's **existence** to a
principal holding its identifier, which is a channel the register carries rather than an invariant
the aggregate breaks — see below.

## What it costs, recorded rather than sheltered

**C17's bound moves, and the register says so.** That row reads *"bounded to items the probing
principal already sees"*, which the zero-count case is outside: the principal sees no member of the
artifact at all. The bound is now **the layer's gate** — a principal who does not reach the layer
learns nothing, by the same single set probe a never-registered name gets — and, within a layer
they do reach, an undeclared criterion discloses existence to a holder of the identifier whatever
their count.

Three things bound it further, and all three are properties of the mechanism rather than
mitigations bolted on:

- **It takes an identifier**, which is not guessable: `tessera_id` is a keyed permutation of entity
  space, so a caller cannot enumerate the artifact space or walk it.
- **It discloses existence and a zero, and nothing else** — no membership, no unmasked size, no
  ordinal, and no other artifact.
- **It is switched off by one field.** A layer declaring any criterion at all closes it, and
  `min_visible = 1` closes it at the weakest setting the schema can express.

## The rule this sets for the routes

The two routes stay different in *what they ask*, and identical in *what decides*. The viewport asks
a geometric question first — is any visible member inside these tiles — because an artifact with no
visible member has nowhere to be drawn; the drill-down asks no geometric question because the caller
named the artifact. **Both then call the same predicate, and neither may hold a different opinion of
it.** A future route that reached a *different* verdict on the same artifact would be the
two-transcriptions failure, and this decision is not licence for one.
