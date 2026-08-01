# 0037 — The canary comparison canonicalises on `tessera_id`, not on `fx_key`

**Date:** 2026-08-01 · **Status:** Settled

## Context

Conformance §4.2 specifies canonicalise-then-compare for I2, and specifies the canonicalisation as
a rewrite of each item handle to its planted `fx_key`. The reason given is that raw comparison "is
impossible, since per-session handle and pin bytes never match".

That reason was true when it was written and is no longer. Decision [0005](0005-tessera-id-keyed-bijection.md)
and decision [0006](0006-per-session-handles-retired.md) replaced the per-session `handle: u32`
with `tessera_id`, a keyed permutation of `(shard_id, entity_id)`; contracts r6 carried it onto the
wire. The points batch is now `(tessera_id, x, y, declared scalars…)`, in which every column is a
deterministic function of the bundle and none is a function of the session. The only per-session
bytes left in a response are the token and `x-tessera-pin`, both of which are headers.

`fx_key` meanwhile remains planted-but-unserved: `tessera-build` writes an empty declared-scalar
set, so no built bundle can carry one. Implementing §4.2 as written would have required connecting
declared scalars end to end in the build first.

## Decision

**Join on `tessera_id`.** The canary fixture builds every state under one identity key, and the
five allocation rules keep each base item's entity id identical across builds — so the same item
carries the same `tessera_id` in every state, by construction. The join §4.2 wanted `fx_key` for is
an equality on a column the wire already carries.

Canonicalisation is then:

- the points batch compared as **its own bytes, in served order** — contracts §3.2 orders the
  served points ascending by `tessera_id` within each tile, so comparing it unsorted is stronger
  than sorting it first;
- the tile batch sorted by tile id and re-serialised, because emission order under a parallel
  gather is not contract;
- nothing stripped, because nothing session-dependent remains in the body.

That last is held by a test rather than by this paragraph: two independently-authorised sessions
with identical visibility must be served identical bytes. If a session-dependent field is ever
added to the body, it fails.

## What this supersedes

**Conformance §4.2's handle→`fx_key` rewrite, and that clause only.** §4.2's normative text still
describes the rewrite; its r6 marker records that the column it defeats no longer exists, and this
decision is the ruling. Nothing else in §4.2 moves — the canonicalisation *procedure* (sort what is
not contract, compare what is, strip transport artifacts) is unchanged, and so is its requirement
that the points batch be compared explicitly.

## Consequence

`fx_key` keeps its purpose and its strict xfail. It is still the designed join for the differential
suites, which need to name an item without an external ID; this decision is about the canary
comparison only, and does not license serving less.

The labels half of §4.2 is uncompared, because there is no label service and no labels batch. When
one arrives, its canonical key is its fixture-unique planted label text, exactly as §4.2 says —
nodes are not items and carry no identity column, so this decision does not extend to them.

## Evidence

`conformance/tests/test_canary.py`; conformance design §4.2 and its r6 marker.
