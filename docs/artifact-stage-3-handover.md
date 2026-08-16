# Stage 3 handover — content and the attachment edge, and the check still owed

**Status:** Live handover. Delete it when Stage 3 closes; the status record is
[`artifact-delivery.md`](artifact-delivery.md) and stays.

**Where the work is:** branch `artifacts/stage-3`, worktree
`.claude/worktrees/artifacts-stage-3`, on top of `16ca0e9` (the last Stage 2 commit). The gate is
green — `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`scripts/check-layers.sh`, `scripts/check-doc-links.py`, and the client suite under `clients/ts` —
with one known-flaky exception noted at the end.

## What is built

**Derived content.** A closed vocabulary — `centroid`, `box`, `hull` — declared per layer and
recomputed per request from `membership ∩ M_auth`. The closure rule is enforced by the type rather
than by care: the visible rows come from `MaskedSet::visible_rows`, which is the only way to obtain
them, so a property computed over full membership has no input to compute from. A name outside the
vocabulary is refused at registration, `extractive_terms` included.

**Supplied content and containment.** An artifact carries ranked variations, each with a generating
set; a viewer is served the first variation whose set they contain **entirely**, or the artifact is
absent. `Containment` is a three-valued enum on purpose — *this layer declares no content* and *you
may not read this content* are different answers, and collapsing them serves the second case with
its description missing (decision 0076).

**Where each half lives.** Content values go in the **record blob**, the store points use, in extents
of their own (decision 0077, and the owner's uniformity ruling of 2026-08-16: artifact properties are
ordinary entity-keyed properties, so filters and search reach them by the route they already reach a
document's). Generating sets go in the **packed extent** beside the membership, because they are what
the serving path does bitmap arithmetic on per request and a compressed home would foreclose that.

**Both directions of the boundary.** `PUT /control/layers/{name}/artifacts` takes a `content` list
per artifact; the kind-5 frame and the drill-down response carry the chosen variation; all three
decoders — Rust, TypeScript, the Python oracle — read it.

**The attachment edge.** An artifact published as an attachment to another is tested on its target's
`verdict` **and** its target's gate, inside the one predicate — so it holds on every route rather
than on the ones that traverse the edge, which is the whole of the fail-open the corpus has caught
twice. The caller names a target by its stable key, since an ordinal never crosses the boundary;
what is stored is the resolved `(layer, level, ordinal, entity)`, so the extra term is one `verdict`
lookup. Publication refuses a target that does not exist yet, and refuses an edge into a layer the
attaching layer did not declare in `depends_on` — the declaration is what makes the refusal of a
dangling replacement sound. **Nothing of the edge crosses the wire**: it is a visibility term, and
traversal is Stage 5's.

## What is owed

### 1. The stage check, on the real corpus — all that is left

`artifact-delivery.md` §5.2 names the fixtures: `topics/ctfidf-2026-08` and
`centroids/kmeans-2026-08` over the 2.4M bundle. The result to produce is the design's own worked
example — **a broad viewer and a narrow viewer failing the *same* full-sample label for the same
reason, and both satisfying its per-term variant** — plus: suppressing a cluster stops its labels
serving on a held identifier, not only on traversal.

Stage 2's equivalent is the model to follow: a script that publishes against a live server, a table
of five principals in `artifact-delivery.md`, and a repro that fails if the numbers stop moving with
the principal.

### 2. Two recorded loose ends, neither reachable today

- **The content extent is absent from `manifest.files`**, so a torn one is unattributable to a
  digest. Every sibling record extent is digested — the flush's on the pool, the coalesce's at
  `coalesce.rs`'s publication — and this one is named and undigested. Its two addressing files *are*
  fsynced now.
- **A node with published artifacts still never folds.** Stage 2's refusal (the prefix-relative
  membership paths) is joined by a second route now that content extents exist: the fold refuses on a
  corpus with no blob-resident column, naming the wrong cause. Both belong to Stage 4's fold artifact
  pass, which is that stage's first measurement.

## What will bite you

**The stale manifest, which has now bitten twice.** Both publication paths clone the *live
generation's* manifest, and a side-manifest write does not swap the generation — so a list that is
*extended* on the clone loses every earlier publication's entries. `membership_extents` and
`artifact_record_extents` are therefore held complete on the executor and **assigned**, never pushed.
The second occurrence was written one line below the comment warning about the first. Any new
per-publication list needs the same posture, and
`two_publications_of_content_both_survive_the_loss_of_the_whole_log` is the shape of test that
catches it — a single publication passes whatever the manifest does.

**Two id regions, and they are load-bearing.** Artifact ids descend from `u32::MAX`, point ids ascend
from 0. That is why artifact and point rows can share one record blob (their has-row bitmaps cannot
overlap, so the stack's disjointness check passes rather than fires), and why *which access rule
governs a row* is a range check rather than something a reader has to remember. The visibility rule
never transfers between them: a document's field is visible to whoever may see the document; an
artifact's content to whoever contains its generating set. `annotations.md` §7's withdrawal is
exactly this line — its storage half survived review, its visibility half was the fail-open.

**A restored variation has no values.** `VariationSet::values` is `None` when the record came back
from a packed extent rather than the log, and such a variation is **withheld** rather than served
short. If you see an artifact silently absent after a restart, that is the branch to check first.

**Entity-space sets lose members in projection.** A generating set is entity-space and permanent; row
space holds only what the slice has folded in. `ProjectedSet` carries the declared cardinality beside
the projected rows for this reason, and a set that lost members contains nobody. Any new set that
crosses into row space needs the same treatment.

**Disk.** Three worktrees at ~40–60 GB of build output each will fill the volume; a link failure with
`ld terminated with signal 7` is the symptom. Stage 1's and Stage 2's target directories have been
removed; regenerate rather than assume.

## Recipes

**The gate** (from the worktree root):

```bash
cargo test --workspace -j 2
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
python3 scripts/check-doc-links.py
cd clients/ts && npm test --workspaces --if-present
```

`a_stalled_or_disconnected_stream_is_shed_and_the_gauge_returns_to_zero` (`tessera-server`, `http`)
fails under the parallel workspace run and passes in isolation. It is load-sensitive, diagnosed in
Stage 2 and recorded on the test itself; re-run it alone before believing a failure.

**Re-capturing the client golden**, which any wire change requires — a decoder test passing against a
stale golden is worse than no test. `clients/ts/README.md` carries the rule; the artifacts golden may
be captured against any corpus with a layer, because it is taken at `k = 0` and holds no points
frame. The scratch recipe used here: generate a 10k-point corpus, `tessera build --mint-id-key
--mint-external-ids`, serve it, register a layer declaring `centroid`/`box`/`hull`, publish clusters
**by quadrant** (a run of consecutive ids samples the whole extent on a modular generator, so every
centroid lands in the middle and a decoder reading row 0 for every row would pass), then capture at
`k = 0` with one named layer.

**A bundle built before a manifest field is added will refuse to open**, by design. Rebuild it; the
refusal names the missing field.

## The review

Two independent lenses ran over `16ca0e9..bd18d64` on 2026-08-16 — disclosure and invariants;
correctness and durability. The disclosure lens found the predicate clean. The durability lens found
the stale-manifest defect above and three lesser ones, all fixed at `a301579` and `1ed15c0`. It also
answered the question the uniformity ruling could not be checked against when it was made: **a
coalesce over an extent holding both point and artifact rows composes correctly, and a fold over one
loses nothing.**

Stages 1 and 2 have had no review but the implementer's own. `/code-review ultra` over the branch is
the owner's to trigger and has not been run.
