# Handover — artifact work, after Stage 5 and the configuration rework

**Date:** 2026-08-20 · **Status:** Stage 5 closed; one tail owed and ruled. Branch
`artifacts/stage-4`.

Two large bodies of work landed on this branch in the same week and met at a rebase: **Stage 5 —
trees, levels and the cut**, and the **configuration rework** with **artifacts from points** on top
of it. Both are done and gate-green. This document is the map for whoever goes next.

**Read [`artifact-delivery.md`](artifact-delivery.md) first** — it is the status record for all
artifact work by owner direction, not GitHub issues, and it wins over this document wherever they
differ. [`artifact-config-handover.md`](artifact-config-handover.md) is the configuration rework's
own handover and is still accurate about its surface; this one does not repeat it.

## 1. Your work list, in order

Three items, and the ordering is the owner's (2026-08-20).

### 1.1 The dependent drop — Stage 5's owed tail

**Ruled, not built.** This is the whole of what Stage 5 still owes, and it is small.

[Decision 0089](decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md) makes a
dependent artifact visible exactly when the artifact it depends on passes its own test.
`Engine::dependency_served` implements that by calling the same `verdict` every serving route calls.
**The cut runs after the verdicts**, and removes artifacts that passed. So a request carrying an
`artifact_budget` over a treed layer, alongside a layer that depends on it, is answered with labels
describing clusters that same response does not contain.

**What to build:** once the response's membership is settled, drop any dependent whose target was
cut. Chains cascade; `DEPENDENCY_CHAIN_MAX` already bounds the recursion.

**Where it goes.** `serve_artifacts` already holds the whole response before resolving parents, in a
`served_at` map keyed by `(layer, level, ordinal)` — which is exactly the triple an `Attachment`
carries. The pass exists because `parent_id` needed it; this rides it.

**The ruling you must not reverse.** The drop happens **server-side and the attachment identifier
never reaches the wire.** Publishing it so a client could filter for itself was considered and
declined, for the same reason `parent_id` carries a null rather than a withheld parent's name:
handing over the identifier names an artifact that is not in the response. A client never told the
relationship cannot notice what is missing from it.

**The trap.** A request naming the dependent layer *alone* — "give me just the labels" — finds no
target in `served_at`, and a naive lookup drops every label. That is a legitimate call and refusing
it is outside the disclosure surface. The condition is: the target's layer **is in this request**
and its target is **not in the response**. One response never contradicts itself; a request for
labels alone behaves exactly as it does today.

**Why this does not make the budget a disclosure control.**
[Decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md) stands. The pass can only
remove, and everything it removes already passed its own test. It decides what is *drawn* — and a
label describing something not drawn is not drawn either.

### 1.2 I3 containment — the conformance gap

Named by the configuration handover as the strongest candidate, and it is squarely artifact work.
The machinery I3 needs **is built**; what is missing is a test, which is a different claim from
*we cannot test this* and only the second is an excuse (`conformance.md` r13 carries the per-row
reasons).

The shape: two principals, one published label, the one missing **exactly one** generating-set
member served *nothing* — not the artifact stripped of its description. Stage 3 proved the
behaviour on the 2.4M corpus; what does not exist is the conformance row asserting it.

Moving this row is the cheapest real improvement available, and it corrects a register that
currently understates itself.

### 1.3 Stage 6 — and it opens with a discussion

**Do not start Stage 6 by writing code** (owner, 2026-08-20).

A predicate membership is answered by a masked scan per artifact. **The cut cannot save it**: the
cut runs after the verdicts, so it serves fewer artifacts and never evaluates fewer. A viewport over
a layer of a few hundred predicate artifacts pays every one of them whatever `artifact_budget` the
client sent — the demo corpus serves 263 HDBSCAN clusters to a broad principal, which is the number
to hold in mind. The budget looks like a cost control here and is not one.

The discussion is whether the per-request predicate bound (delivery §2, owed and unspecified)
carries the whole weight, or whether something filters before evaluation. **The second is the
dangerous half**: anything that skips evaluation on geometry is a disclosure decision taken on a
stamp, which is the shape [decision 0041](decisions/0041-pins-become-a-staleness-stamp.md) already
refused for pins. `membership = { attribute = … }` is declared and unbuilt today, which is the
fail-closed state to start from.

Also still open at Stage 6 and unchanged: ⊘ the proportional criterion's denominator, since *"the
points inside this shape"* declares no member set and its size moves at every write.

## 2. What Stage 5 built, so you do not rediscover it

Its handover is deleted; this is the residue worth carrying.

**The hierarchy is in the edges, and only the parent direction is durable.** Children are derived by
inverting parent edges per level at serve time, so no deletion has to keep two copies of one fact
agreeing. `children_keys` was deleted in the configuration rework for exactly this reason.

**A layer's edges run either within a level or between them, and may not mix**
([decision 0087](decisions/0087-cross-level-edges-are-information-not-rollup.md)). `nested` is the
clustering case; **`tiered`** is the levelled case, the declaration value that had been missing and
the reason the third shape could not be expressed at all. It is now load-bearing beyond Stage 5:
the artifacts-from-points list-column reader dispatches on it, one entry per level under `stacked`
and `tiered`, a lineage under `nested`.

**A budget takes nothing on a tiered layer**, exactly as on a flat one. There is no depth to trade,
because the resolution is the client choosing a level. Substituting a state for its counties is not
the honest coarsening that substituting a parent cluster for its children is.

**The cut climbs to a *passing* ancestor, never to a depth.** An integration test caught the
alternative: a suppressed root blanked its children's regions entirely at a budget of one. If you
touch `cut.rs`, that property is what the differential test against the reference implementation
over 200 random forests is protecting.

**The proportional gap is real and easy to test vacuously.** Under an absolute member requirement
(`require_member_visibility = { count = n }`) a passing child never sits beneath a failing parent;
under a proportional one (`{ fraction = p }`) one does. The first version of that test
passed for the wrong reason — the parent was covered by the frontier rather than failing its bar.
If you write one, publish the parent into a second layer where nothing covers it.

**`parent_id` is on the artifacts frame and its null rule is the disclosure rule** (leak register
**C29**, contracts §3.2). Null means *no parent in this response*, covering both a root and a parent
that exists and was withheld. Do not model a "hidden parent" state; there is nothing to fill it
from. The TypeScript client and the viewer both read it this way, and the viewer's `servedLineage`
treats an unresolved link as no link.

**⊘ Per-branch depth stays unspecified.** A budget resolving to different depths in different
branches is the honest general case; the agreement property two budgets rest on is written for the
single-depth form, so do not add it casually.

## 3. Two corrections owed to documents you will read

**The configuration handover's §8 calls `level` / `attached_level` remapping "a real gap for stacked
and tiered layers".** [`design/configuration.md`](design/configuration.md) decides the opposite, and
gives the reason: a layer's `fields` map is closed, a level is an **address** rather than a value —
it is what makes `(layer, level, key)` an artifact's identity — and a producer whose source spells
it otherwise renames the column. The normative document wins. Correct the handover line rather than
the code.

**`clients/ts/viewer/smoke.mjs`** still carries a stale assurance that `/v1/categories` answers 500
for a `derived` column "because the predicate is ⊘ unbuilt". The predicate is built and the test
tolerates those 500s. Changing what it tolerates is behaviour rather than a rename, so it wants a
look rather than a sed.

## 4. Things that will bite

**The demo corpus is the fixture, and it is re-derivable.**
[`notebooks/arxiv-corpus.ipynb`](../notebooks/arxiv-corpus.ipynb) writes the whole corpus and its
one declaration; [`notebooks/run-corpus.sh`](../notebooks/run-corpus.sh) builds it, serves it and
opens the viewer. Use it before reasoning about behaviour from the types — three of Stage 5's
findings came from running it and none from reading.

**The view's extent is `auto`, and that is load-bearing.** The notebook writes raw UMAP coordinates
and the frame fits a box around exactly those numbers. Hand-scaling them against a stated extent is
what this notebook used to do, and it put the entire corpus in a forty-cell speck in one corner of a
65 536-cell world — silently, with no clamp and no error, and it survived a full review because
every count was still correct. Do not reintroduce a scale factor anywhere.

**`--carry-id-key-from` does not carry the term dictionary.** Term ids are assigned by first
appearance, so a new term appearing earlier renumbers the dictionary, changes the signature sort,
changes permanent entity ids, and changes every `tessera_id`. **Do not rebuild a bundle you intend
to keep identities across.**

**⊘ The artifact's own-terms gate is unbuilt.** No per-artifact term is stored, and a layer
declaring `artifact_visibility = { field = … }` withholds — fail-closed. Comments in the serving
path still point at "Stage 3" as where it arrives; the stage pointer is stale, the gap is real.

**`tessera-engine --test write`** has timing-sensitive deny-latency cases that can fail under load
from a concurrent cargo invocation. Re-run that binary in isolation before reporting one as yours.

**Work in a worktree** — `.claude/worktrees/<name>` on its own branch
([`agents/parallel-work.md`](agents/parallel-work.md)), whether or not anything runs beside you.
Stage 5 and the configuration rework shared a branch and a working tree for a day, and the cost was
paid in unpicking each other's staged files, not in the code.

## 5. The gate

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-clients.sh
python3 scripts/check-doc-links.py
python3 scripts/check-corpus-integrity.py
```

Baseline **1816 Rust tests, 0 failing, 11 ignored**, plus **216 client tests** (5 + 152 + 59, of which 6 are skipped live-service cases).

**`--no-fail-fast`, and read the count.** Without it cargo stops at the first failing binary and
skips the rest, so a run reporting no failures beside a *smaller* passing total reads as success.
That has been mistaken for a green gate here.

## 6. Where authority lives

| | |
|---|---|
| [`artifact-delivery.md`](artifact-delivery.md) | **the status record** for artifact work, by owner direction — not GitHub issues. Move it with the work |
| [`design/configuration.md`](design/configuration.md) | the normative declaration surface: the closed key set, every refusal, the worked example |
| [`design/artifacts-from-points.md`](design/artifacts-from-points.md) | artifacts declared by their points — readers, `value_set`, lineage, growth, the wire column, minting, and §8's open items |
| [`design/annotation-write-cycle.md`](design/annotation-write-cycle.md) | artifact-side write semantics (§6.1); §3.4 is the timing table |
| [`design/annotation-representation.md`](design/annotation-representation.md) | the representation, and §5.0.4 on edges constraining write order |
| `decisions/0080`, `0082`, `0083`, `0087` | the per-artifact test; hierarchy in edges; the request-time budget; the two edge shapes and their uses |
| `decisions/0088`–`0091` | the two visibility axes; the dependency edge; a vocabulary's single axis; build is ingest |
| [`artifact-config-handover.md`](artifact-config-handover.md) | the configuration rework's own map — renames, the input shape, and its open items |

**Refuse only where something leaks or is irreversible.** Both recent bodies of work produced
refusals that had to be unpicked — a `public` label under a gated parent, a list column on a `flat`
layer, a self-parent check that rejected a legitimate taxonomy where an archive has no subclass.
Each looked principled and each foreclosed something a caller legitimately wanted. Outside the
disclosure surface, report the numbers and let the operator decide.

**Delete this document when its work list is empty.**
