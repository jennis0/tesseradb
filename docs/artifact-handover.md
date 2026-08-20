# Handover — artifact work, after Stage 5's tail and the I3 conformance row

**Date:** 2026-08-20 · **Status:** Stage 5 fully closed. **One item left on the list and it opens
with a discussion, not with code.** Branch `artifacts/stage-4`.

Two of the three items this document carried on 2026-08-20 are done. The cut's owed tail is built —
a dependent is dropped when the response does not contain what it depends on — and I3 containment
has moved from *untested machinery* to a covered conformance row. What is left is Stage 6, and the
owner has ruled that it does not start by writing code.

**Read [`artifact-delivery.md`](artifact-delivery.md) first** — it is the status record for all
artifact work by owner direction, not GitHub issues, and it wins over this document wherever they
differ. [`artifact-config-handover.md`](artifact-config-handover.md) is the configuration rework's
own handover and is still accurate about its surface; this one does not repeat it.

## 1. What is left

### 1.1 Stage 6 — and it opens with a discussion

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

### 1.2 Not artifact work, and larger than it looks: the conformance suite is red

Found while writing the I3 row, recorded in [`design/conformance.md`](design/conformance.md) §0 and
Appendix R r14, and **not fixed**. Whoever picks it up is not picking up artifact work.

The suite could not spawn a server at all: `tessera serve` has taken `--deployment` in place of `-c`
since the configuration rework, and `oracle/harness.py` still passed the old spelling, so every
server-backed module died at startup. That one line is fixed. With it fixed, **27 of 432 tests fail
in seven modules**, for two causes, neither a defect and neither a leak:

- **`public` is interned at term `0`**, so every mask-catalogue descriptor's dictionary id is one
  higher than `oracle/catalogue.py` assumes.
- **[Decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md) made the
  within-signature tiebreak the Morton code**, and the mask catalogue is designed so that
  `entity_id == source_id` — which held only while the tiebreak was the source id.

`verify()` does not catch the second, and the reason is worth carrying: its block check compares
posting **sets**, and a within-block permutation preserves a set. The check whose comment says it
proves the identity does not test it at all. A fix needs a real source↔entity bridge in the oracle —
`fx_key` is the one the corpus already plants — and a `verify()` check a set comparison cannot pass
vacuously.

`conformance/tests/test_label_containment.py` builds its own fixture and is unaffected, which is why
the I3 row moves while the suite stays red. **A coverage row is not claimed on a green suite**, and
§0 says so at the site.

## 2. What was built on 2026-08-20, so you do not rediscover it

**The dependent drop.** `serve_artifacts` collects a `Placement` per served artifact — its own
address, its parent's, and the address of what it depends on — and `orphaned_dependents` removes any
dependent whose target's layer is in **this** request and whose target is not in the response. It
runs **before** the parents resolve, so a dropped dependent takes its own name out of `served_at`
and cannot be named as anything's parent. Chains cascade through a worklist over the edges the
response holds. The attachment identifier never reaches the wire, and that is the ruling: handing a
client the identifier names an artifact the response does not contain, which is `parent_id`'s null
rule at the other grain.

**The trap is tested, and it is what stops a naive fix.** A request naming the dependent layer alone
— "give me just the labels" — finds no target and a naive lookup drops every label. That is a
legitimate call, refusing it is outside the disclosure surface, and
`a_request_for_the_labels_alone_keeps_every_label_it_would_have_had` is the guard. Deleting the drop
turns the other two red and leaves that one green, which is what it is for.

**A target outside the viewport is dropped by the same rule.** Rare by construction — a label's
members are the documents it was drawn from — and separating it from the cut case would mean
carrying a reason per absent candidate through a pass that deliberately collapses reasons. Recorded
because it is the one behaviour here that is not the budget.

**The I3 fixture is built backwards from the property's edge**, and that is the whole of why it is
not the mask catalogue's. `oracle/label_fixture.py` plants one entity carrying a term one principal
holds and the other does not, inside the widest generating set and nowhere else — so "one member
short" is a fact about the corpus rather than a hope, and it is asserted from the masked counts
before anything rests on it. Three artifacts of one layer over one membership differ only in which
generating sets their contents were drawn from, so **the same response carries an absence and its
control**: without the control, deleting containment altogether would still leave the narrower
principal seeing nothing and the test reading green.

**The pin is not re-presented in the cache half**, and §4.4 asked for it. Decision 0041 made a
geometry stamp advisory and never authorisation, so presenting one is an ordinary request with an
ordinary answer and could not hold a suppression out either way. What carries session state across
an overlay change is the token, and that is what the test holds fixed.

## 3. What Stage 5 built, so you do not rediscover it either

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
to keep identities across.** §1.2 is that warning arriving from the inside rather than from a
rebuild, over a fixture that assumed the numbering would hold.

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

Baseline **1819 Rust tests, 0 failing, 11 ignored**, plus **216 client tests** (5 + 152 + 59, of which 6 are skipped live-service cases).

**`--no-fail-fast`, and read the count.** Without it cargo stops at the first failing binary and
skips the rest, so a run reporting no failures beside a *smaller* passing total reads as success.
That has been mistaken for a green gate here.

**The conformance suite is not in this gate and is red** (§1.2). It runs in CI, where it has been
failing since the configuration rework. `python3 -m pytest conformance/tests -q` is how to see it;
405 pass, 27 fail, and `conformance/tests/test_label_containment.py` is 16 of the 405.

## 6. Where authority lives

| | |
|---|---|
| [`artifact-delivery.md`](artifact-delivery.md) | **the status record** for artifact work, by owner direction — not GitHub issues. Move it with the work |
| [`design/configuration.md`](design/configuration.md) | the normative declaration surface: the closed key set, every refusal, the worked example |
| [`design/artifacts-from-points.md`](design/artifacts-from-points.md) | artifacts declared by their points — readers, `value_set`, lineage, growth, the wire column, minting, and §8's open items |
| [`design/annotation-write-cycle.md`](design/annotation-write-cycle.md) | artifact-side write semantics (§6.1); §3.4 is the timing table |
| [`design/annotation-representation.md`](design/annotation-representation.md) | the representation, and §5.0.4 on edges constraining write order |
| [`design/conformance.md`](design/conformance.md) | the invariant matrix (§4.6) and, at r14, the suite's own state |
| `decisions/0080`, `0082`, `0083`, `0087` | the per-artifact test; hierarchy in edges; the request-time budget; the two edge shapes and their uses |
| `decisions/0088`–`0091` | the two visibility axes; the dependency edge; a vocabulary's single axis; build is ingest |
| [`artifact-config-handover.md`](artifact-config-handover.md) | the configuration rework's own map — renames, the input shape, and its open items |

**Refuse only where something leaks or is irreversible.** Both recent bodies of work produced
refusals that had to be unpicked — a `public` label under a gated parent, a list column on a `flat`
layer, a self-parent check that rejected a legitimate taxonomy where an archive has no subclass.
Each looked principled and each foreclosed something a caller legitimately wanted. Outside the
disclosure surface, report the numbers and let the operator decide.

**Delete this document when its work list is empty.**
