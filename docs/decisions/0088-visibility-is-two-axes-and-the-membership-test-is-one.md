# 0088 — Visibility is two axes, and the membership requirement is one of them

**Date:** 2026-08-18 · **Status:** Settled (owner ruling)

## Context

The configuration surface had accumulated three spellings for *who may see this* — a category's
`listing`, a layer's `gate`/`ungated`, and `artifacts_carry_own` — and, separately, two tests that
both asked *how much of this object's membership can the viewer already see*: `visible_when`, a
masked-count threshold, and `corpus_derived`, an all-or-nothing containment test on supplied
content. A vocabulary's `per_viewer` listing asked the same question again at a third setting: a
value is visible when the viewer can see **any** point carrying it.

## The decision

**Everything about who may see an object answers one of two questions, and each gets one key.**

- **`visibility`** — which access label must the viewer hold. Its value is a label, with `public`
  reserved (below).
- **`require_member_visibility`** — how much of this object's own membership the viewer must
  already see: `"all"`, `"any"`, `{ fraction = … }`, `{ count = … }`, or `"none"`.

`visible_when`, `corpus_derived` and a vocabulary's `derived` listing are three settings of the
second key and collapse into it. The word `derived` is retired: it meant *any member* on a
vocabulary and *every member* on content — one word, two quantifiers, in adjacent controls.

**`public` is a reserved access label interned at term `0`**, satisfied by every principal **by
construction inside the trust boundary** — not by grant, which would make it depend on grant
hygiene, and not by the plugin, which is caller-supplied. A corpus whose data already carries the
descriptor `public` is adopted, not refused: modelling open-to-all as a real term is an ordinary
design and this system borrows Accumulo's vocabulary, where it is the usual one.

## Why

The two tests differ only in threshold. Keeping them apart forced a caller to learn two vocabularies
for one question and produced the `derived` collision; merging them makes the second axis nameable,
which is what the corpus had been missing — `require_member_visibility` is the first key that says
*this object's reachability is computed from its members*, a thing three keys were doing without
saying.

The name is deliberate: it **requires** that members be visible and never *sets* their visibility. A
container never grants its members anything, and the reverse reading would invert the direction the
system exists to protect.

## What this supersedes

[0075](0075-the-masked-count-is-an-existence-criterion.md) established that the masked-count test is
independent **of the gate**, and that stands untouched: the two axes are orthogonal and an object
must satisfy both. What 0075 also happened to separate — the count test from the containment test —
is merged here.

[0079](0079-the-gate-is-one-flag-not-three-modes.md)'s substance survives under new names.
`artifacts_carry_own` becomes `artifact_visibility = { field, default }`, where the presence of
`field` is the declaration that artifacts carry their own labels and `default` is what one carrying
none gets. Its hazard argument is unchanged: a schema word must not disable a disclosure control,
and under the two axes it cannot, because the membership test lives on its own key and is required.

Appendix C's C27 and C28 followed `artifacts_carry_own` and `corpus_derived` respectively and now
follow one key each under the new names. The register is shorter, not weaker: what it watches is
that the declaration is explicit and undefaulted, which both remain.

## The record

The full surface — the two axes, the reserved label, per-object sources and field maps, the input
grains, and the rulings of 2026-08-18 — is
[`../evidence/memos/2026-08-18-configuration-surface.md`](../evidence/memos/2026-08-18-configuration-surface.md).
This entry records the ruling; the memo is the design, and
[`configuration.md`](../design/configuration.md) and
[`annotation-write-cycle.md`](../design/annotation-write-cycle.md) §6.1 are where it binds, with
[`per-point-attributes.md`](../design/per-point-attributes.md) §3.8 keeping the semantics the
spelling refers to.
