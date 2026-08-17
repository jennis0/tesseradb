# Stage 5 handover — trees, levels and the cut

**Status:** Live handover, written 2026-08-17 as Stage 4 closed. Delete it when Stage 5 closes; the
status record is [`artifact-delivery.md`](artifact-delivery.md) and stays. It supersedes the Stage 4
handover, whose surviving traps are carried below.

**What the stage is:** a layer's lineage lives in its edges, its levels are declared resolutions, and
a viewport returns a cut through the tree that fits what the client can draw. Its pieces and its
three-part check are [`artifact-delivery.md`](artifact-delivery.md) §3, Stage 5; the mechanisms are
[`annotation-representation.md`](design/annotation-representation.md) §6.2–§6.3 and
[architecture §7.5](design/architecture.md), which carries the replacement text (r43) for the
descent this stage does **not** build.

**The one test the stage exists for:** on the real condensed tree, a principal holding only the term
that covers a parent's stray members sees **that parent and no child** — and the build's containment
report named the edge in advance. Everything else here is plumbing around that case. HDBSCAN's
children are subsets of their parents but do not exhaust them (20–25% of points fall out as noise at
each split on the measured corpus), so any construction that assumes a covering hierarchy is wrong
in a way a balanced synthetic tree will never show you.

## The rulings are taken; there is no design pass owed

Unusually for this sequence, **the three decisions this stage turns on are all settled** before it
starts, and they took away more than they added:

- **The frontier is a per-artifact test** ([0080](decisions/0080-the-frontier-is-a-per-artifact-test.md)).
  There is no walk. Store a membership bitmap per node, take one `and_cardinality` against the mask
  per node, serve it iff it clears its own criterion. A node's verdict has **no viewport input at
  all** — which is what dissolves the pan/zoom differencing half of C1 rather than answering it.
- **A hierarchy lives in its edges; levels are resolutions**
  ([0082](decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md)). A treed layer
  declares **no levels** and sits at level 0 on one reserved run. A levelled layer declares them and
  may carry edges too — the administrative case. They are independent declarations, and a level
  number says nothing about lineage.
- **The cut is a request-time budget** ([0083](decisions/0083-the-frontier-is-a-request-time-budget.md)),
  not a depth, met by serving ancestors instead of their descendants and **never by sampling** —
  dropping half the nodes gives a wrong map, not half a map.

The wire shape for the budget already exists and already ships: `artifact_budget` is an accepted
request field, inert on a flat layer, carried by all three frame decoders. Stage 2 built it there
deliberately so this stage would not have to add a field to a shipped frame.

**A budget is not a disclosure control**, and the temptation to treat it as one is the single
largest correctness risk in the stage. The control is the criterion, evaluated per node against
`M_auth`. A shallower cut serves strictly less; a deeper one serves only nodes that already passed.
If you find yourself reasoning about what a budget hides, you have confused it with §8.4's maximum
depth, which *is* a control and is fixed against `M_auth` for that reason.

## The two things that are genuinely hard

**Rollup holds under an absolute criterion and fails under a proportional one, and that gap is the
stage's second check.** A child's members are a subset of its parent's, so under `min_visible` a
child's masked count is never larger and a passing child never sits beneath a failing parent —
rollup obtained with no walk, nobody left with a blank region. Under `min_fraction` a ratio does not
shrink downward: a parent at 5% of 10 000 declared members fails a 10% rule while its child at 50%
of 200 passes it, the child a strict subset throughout. **A run that does not reproduce that gap has
not exercised the proportional form at all**, and the delivery doc says so because it is the easy
test to write and accidentally not run. No disclosure follows either way — each node passed its own
test — but the lineage has holes, and a layer declaring the proportional form must expect them.

**One depth for the whole tree is what this stage builds, and the general case is unspecified.** A
budget that resolves to different depths in different branches is the honest answer for an
unbalanced tree, and it is ⊘. Do not quietly build it: it changes what "a cut" means and the
agreement check in the delivery doc (a shallow cut and a deep cut agree on every artifact both
return) is written for the single-depth form.

## What Stages 1–4 leave you

- **Membership is entity-canonical on disk and derived in row space**, rebuilt when the generation
  moves; the row form covers members holding **base** rows only. That boundary is what keeps the row
  form untouched by flush and merge, and it is fail-closed — a count understates rather than
  overstates. A hierarchy adds nothing here: an edge is not membership, and a parent's bitmap is
  stored, never unioned from its children at request time.
- **The fold rewrites every level whole** into the prefix it publishes, dropping the deletions it
  executes and nothing else, with content extents carried by hard link. Levels are already a
  first-class thing in the durable form (`.tsmb`, offsets non-decreasing, empty blob = hole). A
  treed layer declaring no levels sits on one reserved run — check what `seed_extent_bound` and the
  level-length authority do with a single-run layer before assuming it is the degenerate case.
- **Rule S and Rule F**, unchanged, and the artifact arm is where they meet: a suppression retires
  only on unsuppress and never touches a membership; a deletion retires only at the fold that
  executes it. Edges into a deleted artifact are answered by the **predicate** — an attachment must
  resolve, and a retired ordinal is a hole that resolves to nothing — rather than rewritten, because
  dropping the edge would leave the child unattached and therefore **served**. This stage adds
  parent/child edges to a graph that already has attachment edges; the same rule has to cover both,
  and "drop the edge" will look correct and be fail-open.
- **The strict/permissive declaration and the fold's report** are built and executed. The report is
  written before the flip and outside the prefix, and a report that cannot be written discards the
  fold.
- **The build plane** publishes layers, artifacts and members at volume through the same registry
  and publication the control plane uses. This stage adds build-time containment verification, which
  **reports** violating edges and decides nothing — put it beside the existing build-plane checks,
  not in the serving path.

## What will bite you

**The stale manifest, which has now bitten four times.** Both publication paths clone the *live
generation's* manifest, and a side-manifest write does not swap the generation — so a list that is
*extended* on the clone loses every earlier publication's entries. `membership_extents` and
`artifact_record_extents` are held complete on the executor and **assigned**, never pushed. The
second occurrence was written one line below the comment warning about the first. **An edge table is
exactly the kind of new per-publication list that walks into this**, and
`two_publications_of_content_both_survive_the_loss_of_the_whole_log` is the shape of test that
catches it — a single publication passes whatever the manifest does.

**Ordering that is correct in the steady state and wrong for the duration of the fold's own work.**
Two of the four defects Stage 4's review found were this shape: a retire that ran after the warm,
leaving a window where a deleted artifact read as not-denied with its record present; and a report
written ahead of three discard points. Anything this stage adds to `publish_fold` — and an edge
table will be added to `publish_fold` — needs the same question asked of it.

**A hole is identity, and the top of a level does not seed itself.** A retired artifact's ordinal
stays a hole because an ordinal is identity; a hole in the *middle* survives without help, which is
why the missing one at the *top* hid until review. The extent's `count` is the authority on a
level's length. Edges naming ordinals inherit all of this.

**Two id regions, load-bearing.** Artifact ids descend from `u32::MAX`, point ids ascend from 0.
That is why artifact and point rows share one record blob, and why which access rule governs a row
is a range check. A parent/child edge names two artifacts, so both endpoints live in the descending
region — an edge that ever holds a point id is a bug, not a feature to generalise.

**Entity-space sets lose members in projection**, and the fold is precisely the event that changes
which members project. `ProjectedSet` carries the declared cardinality beside the projected rows for
that reason, and a set that lost members contains nobody.

**A restored variation carries no values.** `VariationSet::values` is `None` when the record came
back from a packed extent, and the serving path reads the record blob at the artifact's own entity;
a blob that cannot answer **withholds the artifact**. If an artifact goes silently absent after a
restart, that is the branch to check.

**⊘ Decision 0072 is settled and unbuilt, and the fold is where it lands.** The moment slot reuse
exists, the fold that frees a slot must first drop it from every enumerated membership naming it —
otherwise a later ingest allocated that slot **silently joins an analyst's stored selection**,
invisible to its owner.

**⊘ There is no live entity → artifact lookup.** Between folds, which artifacts a deletion degraded
is answerable only from the last fold's report. If this stage seems to want that lookup, that is a
design question rather than a missing index.

**⊘ No artifact carries its own terms yet**, so a layer declaring `artifacts_carry_own` serves
nothing on either route. Fail-closed and deliberate.

## Carried over unbuilt from Stage 4

- ⊘ **A retired artifact's content is not reclaimed** — unreferenced bytes in the carried extents.
  Hygiene rather than disclosure, and the fold is where it belongs.
- ⊘ **The fold's report has no HTTP route.** `Engine::last_fold_report` and
  `reports/fold-<prefix>.json` in the bundle root are the whole surface; it is a file and an
  accessor rather than a subscription.
- ⊘ **The read battery does not exist** (`correctness-suite.md` §3 marks the whole thing ⊘), so the
  artifact census stands on its own rather than as one row of a suite. This stage's check is written
  in the battery's vocabulary and will want it.

## Data

**The real clustering's condensed tree is the fixture this stage turns on** (§5.2), because its
non-exhausting splits are a property of HDBSCAN rather than something planted — and the
non-covering case is the middle of the three checks, the one that makes this stage more than
plumbing. A planted tree will not produce it.

**The seeded generator carries everything else.** Its artifact arm is built: membership is a keyed
interval plus a keyed scatter drawn through a keyed bijection, so both directions are closed form —
the members of an artifact, and the artifacts holding an entity — and `artifact_census.rs` asks the
engine both questions over every artifact and every entity, before a write, after a deletion, after
the fold that executes it, and after a restart. **It has no edges.** A treed layer needs an arm of
its own, under the same rules: closed form in both directions (a node's children, and a node's
ancestors), prefix-stable, and planting the cases nothing produces by chance — a node whose children
do not exhaust it, a chain deep enough to make a budget bite, a level with a hole in it.

## Recipes

**The gate**, from the worktree root:

```bash
cargo test --workspace -j 2
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
python3 scripts/check-doc-links.py
cd clients/ts && npm test --workspaces --if-present
```

`a_stalled_or_disconnected_stream_is_shed_and_the_gauge_returns_to_zero` (`tessera-server`, `http`)
is environment-sensitive and **known red on this host** — bisected to a commit predating the
artifacts frame, with the investigation recorded at the test. Re-run it alone before believing a
failure, and do not attribute it to your change.

**A corpus with artifacts already in it:**

```bash
tessera build --points … --pairs … --layers layers.toml \
  --artifacts artifacts.parquet --artifact-members members.parquet …
```

The file shapes are [`annotation-write-cycle.md`](design/annotation-write-cycle.md) §6.1;
`crates/tessera-build/tests/build_layers.rs` writes all three.

**The whole write cycle against a running server**, which is also the fastest way to see the fold's
report earn its keep:

```bash
./run_demo.sh --scale 2m4                       # TESSERA_DATA points at the checkout's data/
cd clients/ts
node scripts/publish-clusters.mjs --presets .dev/presets/2m4.json \
  --clusters 24 --labels topics/demo --label-term 46
node scripts/write-cycle-demo.mjs               # --dry-run first
```

The driver publishes the pair it damages rather than following an existing label — a generating set
never crosses the trust boundary, so a driver following someone else's labels would delete documents
at random until one landed in a sample.

**A worktree has no `data/` and no `node_modules`.** Set `TESSERA_DATA` to the checkout holding
`data/`, and run `npm install` under `clients/ts` before any script. A bundle built by an older
binary will refuse to open once a manifest field moves — that is the fail-closed guard working, and
`--rebuild` is the answer.

**Disk.** Several worktrees at ~40–60 GB of build output each will fill the volume; a link failure
with `ld terminated with signal 7` is the symptom.

## The reviews behind this

Stage 4 was reviewed once, over the whole branch, and found four defects — all in the fold's
publication, two of them the steady-state ordering shape described above. Stage 3 was reviewed
twice, both times by two independent lenses; the second pass found six, every one a way to publish
something nobody wrote.

**Every pass so far has found its worst defect in a reader rather than in a predicate** — in the code
that decides what a caller *said*, not in the code that decides who may see it. An edge table is a
reader. Stages 1–4 have no `/code-review ultra` behind them: the branch was too large for it at the
merge, and a narrower base is the way in.
