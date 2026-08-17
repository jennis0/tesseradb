# Stage 4 handover — the write cycle

**Status:** Live handover, written 2026-08-16 as Stage 3 closed. Delete it when Stage 4 closes; the
status record is [`artifact-delivery.md`](artifact-delivery.md) and stays. It supersedes the Stage 3
handover, whose surviving traps are carried below.

**What the stage is:** ingest, delete, suppress and the fold leave every artifact correct, and the
fold does not resurrect a withheld label. Its four pieces and its check are
[`artifact-delivery.md`](artifact-delivery.md) §3, Stage 4; the mechanisms are
[`annotation-write-cycle.md`](design/annotation-write-cycle.md) §3–§4 and
[`annotation-representation.md`](design/annotation-representation.md) §5.0.3, both normative.

**The one test the stage exists for:** delete a member of a label's generating set, watch the label
vanish at the ack, run a fold, and **it stays gone**. An earlier draft of the design had the fold
re-base generating sets into row space, which returned the withheld label at the fold — served on a
set that no longer named what its text was derived from. That is the fail-open the write cycle was
commissioned to close, and it is the shape to keep testing for.

## Done: the fold's artifact pass

**A node holding artifacts folds now**, which it did not before — and since Stage 3 that state was
every bundle built with `tessera build --artifacts`, so the 10⁷-artifact bundles the build plane
exists for were exactly the ones that could never compact.

**The measurement it owed is run** ([the probe](../probes/2026-08-16-fold-artifact-pass/README.md))
and settled the construction: project each artifact's entity-space membership through the new
`permutation.bin`. Half the posed comparison dissolved first — there is no `old_row → new_row` table
to scatter-build, because membership is entity-canonical and old rows never enter the pass. Against
riding pass 1 it costs +3.5 GB rather than +9.2 GB, in page cache rather than anonymous memory, and
it threads where riding cannot: 32.8 s on eight threads against 101.3 s at 10⁹ rows and 10⁷
artifacts, linear in rows across three decades.

What `publish_fold` does at step 3a: rewrites every level whole into the prefix it is publishing,
one extent per level, dropping the fold's **executed deletions** from each membership and nothing
else — a suppressed member keeps its bit (Rule S) and no generating set is touched. The content
extents are carried by hard link beside them, and the row forms are rebuilt inline after the swap
rather than left to whoever arrives first. Nineteen tests in `artifact_fold.rs`.

**Two stale-manifest defects were found in the doing, both of the class below.** The fold took its
layer registry from the live manifest, which a side-manifest write does not refresh — so it
published a prefix whose membership extents named layers it did not declare, every extent was
skipped at open as a dropped layer's leftovers, and **every artifact came back absent with no error
anywhere**. The content extents had the same shape one field over. Both now read the executor's held
state, which is the posture the online route already takes.

**`plan_fold` budgets it** (ruled 2026-08-17): 90 B per Roaring container, counted from the resident
store when the fold is planned rather than modelled from the manifest. Cost is per *container* — not
per artifact and not per member — so nothing a manifest holds predicts it, and charging per declared
member would overcharge a compact clustering by an order and refuse folds that fit. The two probes
agree the constant independently: the residency sweep measures 78.5–94.0 B flat across three
decades, and the pass measured +3.5 GB where 90 B × 4×10⁷ containers predicts 3.6 GB.

## What Stages 1–3 leave you

- **Membership is entity-canonical on disk and derived in row space** (`ArtifactRows::build`),
  rebuilt when the generation moves. The row form covers members holding **base** rows; a member
  whose row is still in a flush extent contributes nothing until the fold folds it. That boundary
  is what keeps the row form untouched by flush and merge, and it is fail-closed — a count
  understates rather than overstates. ✔ **Built**, and the projection is keyed by prefix, slice and
  store version rather than by the segments version: keying on the version a flush moves rebuilt
  every level on every flush, for a set of bits that had not moved.
- **The deny lane, the overlay, Rule S and Rule F** are the point path's, unchanged, and an artifact
  reaches them by the same route a point does. ✔ **Rule F's artifact arm is built**: a deleted
  artifact's record leaves its level in the publication that retires its overlay entry, and the
  ordinal stays a **hole** because an ordinal is identity. Edges into it are answered by the
  predicate — an attachment must now *resolve*, and a hole resolves to nothing — rather than
  rewritten, because dropping the edge would leave the label unattached and therefore **served**.
  ⊘ A retired artifact's content is not reclaimed: unreferenced bytes in the carried extents,
  hygiene rather than disclosure.
- **Supplied content lives in the record blob**, in artifact extents of its own, written by both the
  online publication and the build. Generating sets ride the packed membership extent beside the
  membership. `G` is entity-space and immutable, so the fold has nothing to re-base — its role is to
  execute the layer's strict/permissive declaration and to produce the report.
- ✔ **The report sweep is built**: one `and_cardinality` per artifact against the deletions this
  fold *executes* — not every tombstone it holds, since a carried-forward deletion has not retired
  and its notice is not yet owed. It is written before the flip and **outside** the prefix
  (`reports/fold-<prefix>.json` in the bundle root), because a fold reclaims the prefix it
  supersedes; a report that cannot be written discards the fold. Control-plane, unmasked counts,
  ⊘ no HTTP route yet — `Engine::last_fold_report` and the file are the surface.
- **The build plane** (`--layers`, `--artifacts`, `--artifact-members`) publishes at volume through
  the same registry, allocator and publication the control plane uses. Anything the fold learns to
  rewrite must handle extents written by *either*, and the file names come from different counters —
  the build's from a fixed zero, the runtime's from `allocate_manifest_n()`. Nothing asserts they
  cannot meet beyond a test that a built bundle takes an online publication beside its own.

## What will bite you

**The stale manifest, which has now bitten four times** — twice in Stage 3's content work and twice
in the fold's artifact pass, where it cost the layer registry and the content extent list. Both publication paths clone the *live generation's*
manifest, and a side-manifest write does not swap the generation — so a list that is *extended* on
the clone loses every earlier publication's entries. `membership_extents` and
`artifact_record_extents` are therefore held complete on the executor and **assigned**, never
pushed. The second occurrence was written one line below the comment warning about the first. Any
new per-publication list needs the same posture, and
`two_publications_of_content_both_survive_the_loss_of_the_whole_log` is the shape of test that
catches it — a single publication passes whatever the manifest does.

**The merge arm was the one a reader leaves out — and it is gone rather than built.** A merge
permutes row space inside its span, so a form holding those row ids would count whichever documents
landed there afterwards: fail-open, and upward, which is the direction that lifts an artifact over
its existence criterion. The base-row rule removes the state it needs, so a flush appends rows the
form does not hold and a merge renumbers rows it does not hold. What pins it is
`a_merge_that_renumbers_extent_rows_disturbs_no_artifacts_count`, which runs a real merge over four
extents rather than asserting the property from the rule.

**Rule F and Rule S must not be conflated, and the artifact arm is where they meet.** A suppression
retires only on unsuppress and never touches a membership; a deletion retires only at the fold that
executes it. An artifact arm that dropped a suppressed member's bit would be giving a suppression a
second retirement route, which is fail-open — the class this corpus has caught twice.

**Two id regions, and they are load-bearing.** Artifact ids descend from `u32::MAX`, point ids ascend
from 0. That is why artifact and point rows share one record blob (their has-row bitmaps cannot
overlap), and why *which access rule governs a row* is a range check rather than something a reader
must remember. A fold that renumbers must renumber the point region only: an artifact has no row.

**Entity-space sets lose members in projection.** A generating set is entity-space and permanent;
row space holds only what this slice has folded in, so a member awaiting a fold projects to nothing.
`ProjectedSet` carries the declared cardinality beside the projected rows for that reason, and a set
that lost members contains nobody. Any new set that crosses into row space needs the same treatment
— and the fold is precisely the event that changes which members project.

**A restored variation carries no values, and that is a pointer rather than an absence.**
`VariationSet::values` is `None` when the record came back from a packed extent; the serving path
then reads the record blob at the artifact's own entity. A blob that cannot answer **withholds the
artifact**. If you see an artifact silently absent after a restart, that is the branch to check —
and note that the fold does not rewrite artifact content extents today.

**⊘ Decision 0072 is settled and unbuilt, and the fold is where it lands.** The moment slot reuse
exists, the fold that frees a slot must first drop it from every enumerated membership naming it —
otherwise a later ingest allocated that slot **silently joins an analyst's stored selection**,
invisible to its owner. The write cycle defines the reconciliation for generating sets only; the
membership clause ships with 0072 whenever it does.

**⊘ There is no live entity → artifact lookup.** Between folds, *which artifacts were degraded by a
deletion* is answerable only from the last fold's report. If something in this stage seems to want
that lookup, that is a design question rather than a missing index.

## The rulings the stage wanted first — both taken

- ✔ **The attachment term does not extend to the target's own criterion** —
  [decision 0086](decisions/0086-the-attachment-term-does-not-inherit-the-targets-criterion.md),
  ruled 2026-08-17. The predicate is unchanged. What it costs is stated there: a label layer over a
  gated cluster layer should declare a criterion at least as strong as its target's, and nothing
  enforces that.
- ✔ **The strict/permissive declaration is built**, strict by default. The fold executes it: under
  `withdraw_content` a variation that lost a source is dropped whole, and an artifact left with no
  variations on a content-declaring layer is **absent** rather than served bare — decision 0076
  reached from the write side. Under `shrink_generating_set` the member leaves the set and the
  description serves again. Publish-time validation is beside it: a declared member or content
  source that is *deleted* refuses the batch, one that is *suppressed* is accepted. The validation
  has no build-plane meaning — a bundle straight out of `tessera build` has no overlay — so it lives
  on the online route alone.

## Data

**The seeded generator is the only fixture that can carry this stage** (§5.1): the stage battery
compares recorded answers across eight stages at sizes where no expectation can be stored, so both
directions have to be closed-form — the members of an artifact, and the artifacts holding an entity.
`tessera-corpus` gains the artifact arm in Rust, under the rules the rest of it already obeys.

The real 2.4M corpus (§5.2) is what the earlier stages were demonstrated on and it cannot carry this
one: the check is "nothing is missing or extra" over all *n*.

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
is load-sensitive and has failed under the parallel workspace run before; re-run it alone before
believing a failure.

**A corpus with artifacts already in it**, which is now the cheap way to get a fixture for fold
work:

```bash
tessera build --points … --pairs … --layers layers.toml \
  --artifacts artifacts.parquet --artifact-members members.parquet …
```

The file shapes are `annotation-write-cycle.md` §6.1; `crates/tessera-build/tests/build_layers.rs`
writes all three and is the fastest way to see them.

**A bench fixture that refuses to open** is a bundle older than a manifest field — by design.
`scripts/bench_build_fixtures.sh --scales 2422486 --label-sets categories-subclass` rebuilds the
2.4M corpus in about ten seconds.

**Disk.** Three worktrees at ~40–60 GB of build output each will fill the volume; a link failure
with `ld terminated with signal 7` is the symptom. Stage 3's debug output was removed when it
closed, so expect a cold build.

## The reviews behind this

Stage 3 was reviewed twice, both times by two independent lenses — disclosure and invariants,
correctness and durability. The first pass (2026-08-16, over the content work) found a data-loss
defect where a second publication un-named the first's content extent. The second (2026-08-16, over
the attachment edge, the corpus check and the build plane) found six, every one a way to publish
something nobody wrote: a disclosure control that could be omitted on one route and not the other, a
mistyped key that published a phantom artifact, publication running in alphabetical rather than
declaration order, a null read as entity zero, a null read as an empty description, and an edge that
depended on row order. All fixed at `5b5b260`.

**Both passes found their worst defect in a reader rather than in a predicate** — in the code that
decides what a caller *said*, not in the code that decides who may see it. Stages 1–3 have no
`/code-review ultra` behind them; that is the owner's to trigger.
