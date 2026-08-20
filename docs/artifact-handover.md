# Handover — artifact work, after Stage 5's tail and the I3 conformance row

**Date:** 2026-08-20 · **Status:** Stage 5 fully closed, the conformance suite green, and Stage 6's
cost discussion held and measured. **What is left is Stage 6's implementation**, which now has its
shape. Branch `artifacts/stage-4` (the conformance fix is `conformance/fixture-drift`, merged into
it).

All three items this document carried on 2026-08-20 are done, and so is the finding they turned up.
The cut's owed tail is built — a dependent is dropped when the response does not contain what it
depends on — I3 containment has moved from *untested machinery* to a covered conformance row, the
suite that row lives in runs green again, and Stage 6's opening question is answered by a
measurement rather than by an argument.

**Read [`artifact-delivery.md`](artifact-delivery.md) first** — it is the status record for all
artifact work by owner direction, not GitHub issues, and it wins over this document wherever they
differ. [`artifact-config-handover.md`](artifact-config-handover.md) is the configuration rework's
own handover and is still accurate about its surface; this one does not repeat it.

## 1. Where the work stands

**Nothing on this list is blocking.** §1.1 is Stage 6, whose implementation is now specified enough
to start; §1.2 is closed and is here for what it taught rather than for what it owes.

### 1.1 Stage 6 — its cost discussion is held, and the answer is measured

**Do not start Stage 6 by writing code** was the owner's direction (2026-08-20), and the discussion
it called for is done. **A predicate layer's row form is cached and rebuilt at a generation move**,
exactly as an enumerated layer's is, so its per-request cost is Stage 2's and the predicate is
invisible to the serving path.

The measurement is
[`predicate_membership_cost`](../crates/tessera-bench/src/bin/predicate_membership_cost.rs) and it
is not close. Deriving per request by crossing the posting lists into row space — which is what the
filter surface's row route does for a leaf — costs **7 900×** a cached count at the demo corpus's
263 artifacts over the whole map: 6.9 seconds against 0.9 ms. A crossing's cost is
`rows × artifacts` where a cached count's is `containers × artifacts`; one crossing serves every set
(decision 0062's rule holds), but each set still costs a probe per row, and a layer is exactly a
collection of sets. The ~5% crossover that makes the row route right for a *single* leaf therefore
never arrives — the tenth-of-the-map column is that route's best case and still loses by three
orders of magnitude, and the gap **widens** with the layer — 3 700× at 64 artifacts, 83 000× at a
million, where it is eleven hours per request. **The caching arm's whole generation-move cost is
repaid by one request**, 15 ms at 263 artifacts.

Two things follow, and both are worth carrying into the implementation.

**The per-request bound is a response-size guard, not a cost control** at the sizes a layer is
normally published at. A bound is about how much a client gets back; caching is about what the
server spends getting it. ⊘ The bound itself stays unspecified (delivery §2) — but the measurement
fixes two things about its form.

*It has to bite well below a million artifacts.* At 10⁶ predicate artifacts over 10⁷ points the
cached route costs **476 ms per request** over the whole map and **15.5 s per generation move**, and
the cut reduces neither — it runs after the verdicts, so it serves fewer and evaluates exactly as
many. A few thousand is comfortable: 25 ms at 1 024, 109 ms at 10 000. Memory is the one resource
that behaves, 108 MB for that million.

*And it must refuse on the layer's declared artifact count, before any evaluation* — never on the
principal's visible count, which needs the evaluation the bound exists to avoid, and which would
make the refusal vary by principal and so become a channel of its own. A total artifact count is
corpus-wide and identical for everyone.

**Filtering before evaluation is refused and need not be argued again.** Anything that skips
evaluation on geometry is a disclosure decision taken on a stamp — the shape
[decision 0041](decisions/0041-pins-become-a-staleness-stamp.md) already refused for pins — and the
measurement removes its motive: the route that would have justified it is three orders of magnitude
the wrong side of the one needing no such filter.

The honest third shape, for whoever finds the invalidation rule hard: derive per request by
*projecting* rather than crossing, which is the caching arm's work done per request instead of per
move. 15 ms per request at 263 artifacts, ~15× the cached cost. It loses whenever a generation
serves more than one request.

What the measurement does **not** cover, stated rather than glossed: a **spatially clustered**
predicate — a boundary whose members share a region — where the crossing's domain is small and the
projection's cost is unchanged. That case favours the crossing and is unmeasured.

What the discussion was called for remains true, and is why it was held first: a predicate
membership is answered by a masked scan per artifact and **the cut cannot save it**, because the cut
runs after the verdicts and so serves fewer artifacts while never evaluating fewer. The budget looks
like a cost control there and is not one.

Also still open at Stage 6 and unchanged: ⊘ the proportional criterion's denominator, since *"the
points inside this shape"* declares no member set and its size moves at every write.

### 1.2 The conformance suite — found red, now green (done)

Found while writing the I3 row and **fixed** on branch `conformance/fixture-drift`
([`design/conformance.md`](design/conformance.md) r14 diagnosed it, r15 is the fix). 432 of 432
pass. Nothing in the engine moved: the diff is eleven Python files, every one of them the fixture
learning something about the build it had been assuming.

Two things are worth carrying out of it.

**The suite could not spawn a server at all** — `tessera serve` has taken `--deployment` in place of
`-c` since the configuration rework, and `oracle/harness.py` still passed the old spelling — so no
module had run since that landed, CI included. A suite that is not in `CLAUDE.md`'s gate can die
silently for weeks.

**A check that compares sets cannot see a permutation.** The mask catalogue is designed so that
`entity_id == source_id`, and
[decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md)'s Morton tiebreak ended
that. `verify()`'s block check compared each block's postings against its entity range **as a set**,
which a within-block permutation preserves exactly — so the check whose comment said it re-derived
the identity had never tested it, and every per-item join in the suite was silently comparing one
item's planted value against another's. It now compares the two spaces item by item.

One fixture-shape fact came with it and no bridge fixes it: **an entity id's position inside its
block now tracks its position on the map**, so a test denying "the lowest 2,400 entities" was
denying a contiguous region.

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

**The conformance suite is not in this gate**, and it is worth running anyway (§1.2):
`python3 -m pytest conformance/tests -q` — 432 pass, of which
`conformance/tests/test_label_containment.py` is 16. It runs in CI, where it had been failing since
the configuration rework without that being visible here. ⊘ `conformance/suite` needs Python 3.11+
for `tomllib` and does not run on this machine.

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
