# 0085 — The existence criterion has no deployment-wide form

**Date:** 2026-08-16 · **Status:** Settled

## What this answers

`min_visible_members` was the architecture's name for the threshold below which a grouping is not
served (§7.5), and it was a **deployment-wide** required config key: absent `[disclosure]` section,
absent key, refusal to start. It was parsed at startup and read by no handler — ⊘ specified and not
implemented — because nothing existed to threshold until artifacts did.

[Decision 0075](0075-the-masked-count-is-an-existence-criterion.md) then moved the threshold into
the annotations design as the **existence criterion**, declared **per layer**, in an absolute and a
proportional form, with **no default**. Architecture r43 left the two facing each other and named
Stage 2 as the place to reconcile them. Stage 2 built the per-layer criterion; this is the
reconciliation.

## The decision

**The criterion is per layer and has no deployment-wide form. The config key is deleted rather than
wired.** `[disclosure]` stays required and holds `token_max_lifetime` alone.

## Why a deployment default is not available

[Decision 0084](0084-an-undeclared-criterion-declares-no-test.md) rules that a layer declaring no
criterion declares **no test**. A deployment-wide floor would have to change that reading — an
undeclared criterion would mean *inherit the deployment's floor* — and then a layer that genuinely
wants no test could not say so: the schema's way of declaring nothing would have become the way of
declaring the default. The alternatives are worse rather than better. A third state (declared-empty
versus absent) buys back the expressiveness by making a declaration's meaning depend on a
distinction the operator cannot see in the file. A floor applied on top of whatever the layer
declares makes `min_visible = 1` a lie for the layer that wrote it, and leaves two
identically-declared layers behaving differently for a reason no reader of either declaration can
recover — the same objection 0084 sustains.

The proportional form settles it independently: it has no deployment-wide parameter at all, and a
single absolute key cannot express a default for a criterion that is not always absolute.

## What the key was actually protecting, and where that goes

The stated point of the key was never its value but the **startup rule**: a deployment must state
its disclosure parameters rather than inherit them. That obligation is discharged where the
parameter now lives. A layer registration carries its criterion or declares its absence, with no
default at either end, and the registry refuses a declaration it cannot read. The obligation moved
with the parameter; it was not dropped.

Nothing is lost by deleting rather than deprecating: no deployment exists
([decision 0048](0048-no-deployments-exist-so-delete-rather-than-support.md)), so a stale key in a
hand-written `tessera.toml` is a key nobody has.

## What this costs

An operator can no longer set one floor for every layer in a deployment. If that turns out to be
wanted, the shape it should take is a **registration-time default applied at declaration** — the
value materialising into each layer's stored declaration as it is registered, so what governs is
still exactly what the layer says — and not a serving-time floor read behind the declaration's
back.

## Consequences

- §7.5's threshold stops being ⊘ specified-and-unimplemented: the control runs, per layer, from
  Stage 2 (architecture r45).
- `Config::min_visible_members` and `AppState::min_visible_members` are removed, with the
  `#[allow(dead_code)]` that had been holding the latter up. `ConfigError::MissingDisclosureKey`
  survives — `token_max_lifetime` still has no default.
- Every `tessera.toml` in the repository loses the key: the demo runner, the conformance driver,
  the bench sweep and the config fixtures.
