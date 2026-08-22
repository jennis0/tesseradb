# Artifacts — delivery and status

**Status:** Living. **This file is the status record for the artifact work** — the stages, what each
must be true to be finished, and where it has got to. Tracked here rather than in GitHub issues by
owner direction (2026-08-15).

**The rule that keeps it true: it moves in the change that moves the work.** A stage that landed
without this file changing is a stage whose status is now wrong, and a file that disagrees with the
code is the file that is wrong. That is the whole discipline — this repository has previously
accumulated plans whose boxes were unticked while the work was complete, because completion lived
somewhere else.

**How the work is done: each stage is implemented in its own git worktree** (owner direction,
2026-08-15), on the convention [`agents/parallel-work.md`](agents/parallel-work.md) already
carries — `.claude/worktrees/<name>` on its own branch, with the track marker and the allowlist —
whether or not anything is running beside it — so that a stage's diff stays separable and the main
tree stays clean. It earned itself on Stage 5, where a configuration rework and an artifacts-from-
points design landed on the same branch while the cut was being built, and the two bodies of work
met only at a rebase.

**Picking the work up:** [`artifact-handover.md`](artifact-handover.md) carries the work list, the
traps in what is already built, and the two corrections owed to documents you will read on the way.
[`artifact-config-handover.md`](artifact-config-handover.md) is the configuration rework's own map
and stays accurate about its surface.

**Reads with:** [`design/annotations.md`](design/annotations.md) (the model),
[`design/annotation-representation.md`](design/annotation-representation.md) (the representation),
[`design/annotation-write-cycle.md`](design/annotation-write-cycle.md) (the write cycle) — all three
**normative** as of 2026-08-16 (decisions 0074–0083; the architecture amendments they owed are
performed at r43);
[`design/correctness-suite.md`](design/correctness-suite.md) §12 (the fixture
machinery this extends); [`probes/dataset.md`](../probes/dataset.md) (the corpus);
[`probes/2026-08-15-artifact-representation/`](../probes/2026-08-15-artifact-representation/) (the
campaign every sizing figure below comes from). Ordering precedent:
[`design/records-and-search.md`](design/records-and-search.md) §13.

---

## Where it stands

A clustering is live end to end, tree and all: layers register, artifacts publish, and a viewer sees
each cluster with the count its own visible set generates, described by the first content whose
generating set they contain entire — on the engine, on all three frame decoders, and on the map.
Edges, levels and the cut are built, the write cycle keeps all of it true across a delete and a
fold, and one configuration file declares the corpus. **There is no frontier and there is not meant
to be**: it was withdrawn for a per-artifact test with a request-time budget (decisions 0080, 0083).
A membership now **grows** as well as being published whole, and **a point says which artifacts it
belongs to on the wire** — an ingest batch may carry a column named for a layer, read by the same
rules a build reads a member table by, joined in the same commit as the rows
([decision 0091](decisions/0091-build-is-ingest-into-an-empty-database.md)) — and under
`value_set = "open"` a key that names nothing **mints** the artifact it names, at the window's
close, which was 0091's last obligation. The cut's owed tail is closed: a dependent artifact is dropped
when the response does not contain what it depends on, server-side, with the attachment never
reaching the wire (Stage 5 below). **I3 containment is a conformance row rather than a claim** —
`conformance.md` r14 moves it to covered on a black-box test whose two principals are one entity
apart. What does not exist yet is membership by predicate, the serving layouts, runtime
artifacts, and the filter and search work — Stages 6 to 9 below.

**Stage 6's cost discussion is held and measured, and its rulings are taken** (2026-08-21). A
predicate layer's row form is cached at a generation move; a layer that partitions is served
row-major instead, which is the layout that scales; the layout is chosen automatically and
re-evaluated at each fold ([decision 0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md));
containment comes from a build-time partition over terms rather than from anything held per token
([decision 0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md));
and **there is no declared bound** — a layer that will be slow says so in the build's report and the
operator decides ([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)),
which withdraws the per-request refusal this file has owed since the model was written. **Scale
validation is pulled forward to Stage 7**, ahead of runtime artifacts (owner, 2026-08-21). The
campaign's plan is [its own memo](evidence/memos/2026-08-21-artifact-scale-plan.md); three of its
tracks are in flight. **Its adversarial review has run and is dispositioned**
([the record](evidence/memos/2026-08-21-artifact-serving-scale-review.md)): two fail-opens found in
the unbuilt design and amended — the settled half of the candidate walk tested containment where it
needed the mask, and the containment partition's deny correction is its acceptance test rather than a
refinement — 0093 amended with the owner's row-major exception, and two leak-register annotations
approved (architecture **r49**). **The grids the review superseded are re-measured** (2026-08-22):
the omitted masked work was real and large, the design's conclusion holds anyway, and the campaign's
expression census says the containment partition's sharing exists only for per-term-authored
generating sets.

| Stage | State | Finished when | Evidence |
|---|---|---|---|
| **0** Rulings and promotion | **done** 2026-08-16 — decisions [0074](decisions/0074-row-less-entities-are-allocated-downward.md)–[0083](decisions/0083-the-frontier-is-a-request-time-budget.md) | the three designs are normative and the register carries their rows | [the review](evidence/memos/2026-08-15-artifact-design-review.md), ten rulings, and architecture **r43** — §7.5's descent and §7.7's ladder amended, §8.4's second threshold withdrawn, C1 and C17 annotated, C27 and C28 added. Five ⊘ items stay open **inside** the normative documents, each allocated to the stage that needs it |
| **1** The spine — allocation and the layer registry | **done** 2026-08-16 (`artifacts/stage-1`) | an empty layer is reachable by gate, suppressible at the ack, droppable for ever, and survives restart | **all five bullets built and gate-green.** The tiebreak in both build paths, verified on the real 2.4M corpus; the two-region allocator with both marks durable; the registry seeded from the manifest and replayed over; `PUT`/`DELETE /control/layers`; `/v1/meta`'s gate-filtered list. Eleven tests, of which the disclosure one is that a gate-failed name and a never-registered one are **one identical set probe** |
| **2** One flat level, masked counts | **done** 2026-08-16 (`artifacts/stage-2`) | two principals get different counts for one real cluster, neither equal to its size; below-criterion artifacts are indistinguishable from absent ones | **met on the map.** One 24-cluster k-means over the 2.4M bundle: the same cluster is 4 / 485 / 1,962 / 4,138 / 8,380 members to five principals against 11,008 declared, and under a `require_member_visibility` of `{ count = 1000 }` the same membership serves them 0 / 0 / 8 / 20 / 24 clusters. Engine, server and all three frame decoders; `@tessera/client` and the viewer; one ⊘ open below |
| **3** Content — derived, supplied, containment | **done** 2026-08-16 (`artifacts/stage-3`) | both principals fail the same real label and both satisfy its per-term variant | derived geometry (`centroid`/`box`/`hull`), the containment test and **the attachment edge** built, published, served and decoded on all three readers, with content crossing the boundary in both directions. **Reviewed 2026-08-16** — one data-loss defect found and fixed (a second publication un-named the first's content extent), three lesser ones with it. **The check is met on the 2.4M corpus** (§3): a principal seeing 7.5% of it is served no label where one seeing 0.6% is served the description, and suppressing a cluster stops its labels on the identifier route. Layers, levels and bulk publication are definable at build time as well as online |
| **4** The write cycle | **done** 2026-08-17 (`artifacts/stage-4`, merged to `main`) | a deleted source document's label vanishes at the ack and **stays gone** across a fold; the stage battery covers the artifact surface | **met on the 2.4M corpus**, driven over the control and viewer planes of a running server: a label published from three documents goes absent the moment one of them is deleted and is still absent after the fold, and the fold's report named the five *other* published layers the same document degraded. **The fold's artifact pass is measured and built** — a node holding artifacts folds, its memberships are rewritten into the new prefix minus what the fold retired, its content is carried, and the row forms are rebuilt inside the fold ([the probe](../probes/2026-08-16-fold-artifact-pass/README.md); nineteen tests). Two stale-manifest defects found in the doing, both of the class that has bitten twice. `plan_fold` prices the pass at the measured 90 B per container, counted from the resident store. **Rule F's artifact arm** is built with it: a deleted artifact's record leaves its level in the publication that retires its overlay entry, its ordinal held open as a hole, and its labels stay withheld because an attachment must now resolve. **The row form covers base rows**, which deletes the flush-union and merge-rebase arms rather than deferring them, and the **report sweep** discharges the notification obligation before anything retires. The **strict/permissive declaration** executes at the fold, with publish-time validation beside it. The generator has its **artifact arm** — closed form in both directions — and the **census** runs the whole surface against it, before a write, after a deletion, after the fold and after a restart. Open: content reclamation, and the read battery this census should eventually be a row of (⊘ no battery exists) |
| **5** Trees, levels and the cut | **done** 2026-08-18 (`artifacts/stage-4`); **its owed tail closed 2026-08-20** | a passing child sits beneath a failing parent under the proportional criterion and never under the absolute one, and two budgets agree on every artifact both return | **all three checks met, the third on the real condensed tree** ([the probe](../probes/2026-08-18-condensed-tree/README.md)): a principal holding only the term covering a parent's stray members is served that parent alone, masked count exactly the 687 members they can see, none of its children, at every budget — and the build's report named the split in advance. **124 of the tree's 131 splits are non-covering**, so that case is the majority rather than the edge. A hierarchy's edges are inline on the artifact record — the parent direction durable, the child direction built per level at serve time, so no deletion has to keep two copies of one fact agreeing. The cut runs after the verdicts and can only serve fewer: where a parent and a child both pass the child is drawn, and a budget is met by climbing to a **passing** ancestor rather than to a depth — the defect an integration test caught, where a suppressed root blanked its children's regions. The proportional gap is proved rather than assumed: the first version of that test passed vacuously, the parent being covered by the frontier rather than failing its bar. The generator has its **edge arm** and the build a **coverage report**; the cut is measured ([`artifact_cut_cost`](../crates/tessera-bench/src/bin/artifact_cut_cost.rs)) at 0.6 ms per ten thousand visible artifacts. The corpus is re-derivable end to end from [the notebook](../notebooks/README.md). **Levels are closed too:** a layer's edges are now declared to run either within a level or between them and may not mix, the second shape being the missing declaration value `tiered` ([decision 0087](decisions/0087-cross-level-edges-are-information-not-rollup.md)) — and a budget does not climb a between-levels edge, because substituting a state for its counties is not the honest coarsening substituting a parent cluster for its children is. What those edges carry instead is **structure on the wire**: each artifact names its parent where that parent is in the same response (**C29**), and the viewer nests what it lists and lights a subtree when one is opened. The demo corpus publishes all three shapes — flat, nested and tiered — over the same points. **The tail is closed** (2026-08-20): a dependent is dropped when this response does not contain what it depends on, chains cascading, **server-side with the attachment never reaching the wire** — handing a client the identifier would name an artifact the response does not hold, which is `parent_id`'s null rule at the other grain. The condition is that the target's layer is in **this** request and its target is not in the response, so a request naming the dependent layer alone — "give me just the labels" — is answered exactly as before, which is the trap a naive lookup falls into. It does not make the budget a disclosure control ([decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md) stands): the pass can only remove, everything it removes passed its own test, and a label describing something not drawn is not drawn either. Three tests: one artifact of three served at each of three budgets and always the one whose subject that cut serves; the labels-alone request unchanged; a note on a label going with the label. ⊘ Per-branch depth stays unspecified |
| **The configuration surface** | **stages 1–8 done** 2026-08-18/19 (`artifacts/stage-4`) — [decision 0088](decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md) | one document declares the corpus and `tessera build` takes no flags; every retired key is refused rather than aliased | **built and gate-green.** Two visibility axes in place of six keys, `public` interned at term `0`, points carrying their own labels, the plugin taking a term list with the manifest hash enforced, one row per artifact with ranked `contents`, `[layer.labels]` expanding to a real layer, `tessera check`, the frame report and `disclosure.json`. ⊘ Stage 9 — the notebook and the corpus's citations — is the remainder |
| **Artifacts from points** | **all five stages done** 2026-08-19/20 (`artifacts/stage-4`) — [the design](design/artifacts-from-points.md) | a clusterer's own output builds a layer: one integer per point or one list per point, noise included, and no artifact table required | **§2, §3 and §4 built and gate-green.** The membership route needed no new surface and is asserted so — a `[layer.members]` block over the points file builds the same bundle, byte for byte, as the same layer declared with a member table. Added in the first stage: an integer key column canonicalised to its decimal spelling (converted once per artifact, never per point); a null key and exactly `-1` skipped, counted and reported rather than refusing the build; and `value_set = "open" \| "closed"` on `[[layer]]`, closed being today's roster rule, open making a key no artifact declares create one. **In the second, a list key column**, whose entries are the artifacts the point belongs to and whose positions mean what the layer's `hierarchy.kind` already declares — one per level under `stacked` and `tiered`, a lineage under `nested`. The same byte-for-byte assertion holds both ways: a lineage column against an artifact table with a `parent` column, and a fixed-length column against a member table with a `level`. A child named under two different parents refuses the build; a shape disagreeing with the declared kind refuses rather than guessing; a null or `-1` entry places the point at no artifact at that level and links nothing across itself. **In the third, a membership grows** ([decision 0091](decisions/0091-build-is-ingest-into-an-empty-database.md) ruled the contradiction r4 found: a point ingested into an enumerated membership joins it, because a build reading a member table has always done exactly that). Three pieces: a durable WAL record carrying a **delta** rather than a restated set — restating a 10⁸-member cluster costs ~12 MB on the fsync path per batch naming it — one store method taken by both the live path and replay, and the packing bookkeeping, which is the part that decides whether it is correct. A level is packed only above its published high-water, so a grown record below that mark reaches a manifest by no append-only route: the log is pinned at the growth and released only by the fold, which rewrites every level whole. Releasing it at the mark that covers a packed tail is the silent failure — the artifact comes back from a restart at its pre-growth size, acked, indistinguishable from one below its criterion — and it is asserted where it bites: **a rotation may not reclaim the member holding a growth**, and after a fold the membership comes back whole with the log deleted outright. Growing a suppressed artifact leaves it suppressed, the key resolving in the store rather than in what is served. Fourteen tests. **In the fourth, the wire says what a file says** (contracts **r36**): `/control/ingest` accepts a column **named for the layer** — its own `name`, as an attribute column is named for the attribute's `name` and not its `field`, which stays build-time acquisition — carrying a key or a list of keys at exactly the values a member table carries, `-1` and null included. The keys resolve at **admission**, so one caller's typo refuses that batch alone rather than the window it would have joined, and the joins become one `ArtifactGrow` per `(layer, level)` inside the batch's own fsync: there is no state in which a point is ingested and its membership is not. **The rule is shared and the reader is not** — `tessera-types` carries no `arrow` dependency and does not acquire one, so `ListMeaning`, `parent_edges` and `integer_key` move there and each side decodes its own Arrow; the build's member pass now reads its list meaning, its noise sentinel and its adjacency from the same three. A lineage **checks** rather than creates: a contradiction is the build's own two-parents refusal, and an edge the layer holds no parent for is reported with the memberships still applied. **The test is 0091's own** — the same corpus built from a member table and ingested with a membership column is the same database to every client, at a scalar key and at a lineage, for three principals, with the built side pinned to an oracle computed from the fixture; dropping the column from the handler's submission turns both red. Ten tests over HTTP, three more on the shared rule itself. **In the fifth, a key that names nothing creates it** (contracts **r37**), which was
[decision 0091](decisions/0091-build-is-ingest-into-an-empty-database.md)'s last obligation and with it the last difference in what can be *said* at the two entry points. Under `value_set = "open"` a membership column's key that no artifact holds mints an artifact carrying nothing but its name, the points of that batch that named it, and whatever computed content follows. **Minting is a publication and it happens at the commit window's close**, not at admission: an ordinal is claimed from the level's cursor and is durable only in the record that claims it, and a `PublishArtifacts` command executes while a window is open and reads the same cursor — so a claim made at admission would be taken twice. What stays at admission is every refusal that can be made about one batch alone, which is what keeps one caller's typo off another caller's rows: a closed layer's unknown key, a layer whose declaration a minted artifact could not satisfy (supplied content kinds, or `depends_on` — the two refusals a publication already makes of an artifact carrying only a key), and a child the batch's own column named under two parents. **A lineage needed no new record shape**: a growth is a delta of members and carries no edge, but the publication that *creates* an artifact has carried `parent_key` since artifacts existed — so a minted chain needed only the order, one record for a nested lineage where a sibling resolves inside its own batch, and one record per level coarse-first for a tiered chain where the parent's ordinal was fixed by the record before. That is `annotation-representation.md` §5.0.4's constraint applied to a batch. **The design's one fail-open is closed three times**: the resolution at admission and the re-resolution at the close both read the store's key index, which no suppression touches, and `prepare_publish` refuses a key its level already holds — so breaking both resolutions together turns the suppression test red on a *refusal* rather than on a second artifact. **What a batch minted is reported to the batch that minted it** — `minted` in the 200, a count in the build's own report, and no bound anywhere, because a bare clustering legitimately creates every artifact it has. Seven more tests over HTTP, of which four are 0091's comparison run again with a seed holding almost none of the clustering, so the tail creates it: at a scalar key, at a nested lineage, at a tiered chain, and against the identity rulings (one key minted once however many points name it; a suppressed artifact's key never minted again; a deleted key returning as a new artifact). **One thing the design had wrong**: the edge-with-no-parent warning was said to stop being reachable once minting landed, and it does not — a child that already exists and holds no parent is still an edge a growth cannot create, whoever its parent is |
| **6** Predicate membership and the serving layouts | not started — **its cost discussion is held and measured**, 2026-08-20; **its rulings taken and its review dispositioned**, 2026-08-21 | one layer built by rule and by list returns identical masked counts for every principal and every viewport, and a layer with no row-space locality is served by a layout that does not walk its artifacts | **the row form is cached at a generation move**, as an enumerated layer's is ([the probe](../crates/tessera-bench/src/bin/predicate_membership_cost.rs)): deriving per request by crossing costs **7 900×** a cached count at the demo corpus's 263 artifacts — 6.9 s against 0.9 ms — because a crossing is `rows × artifacts` where a count is `containers × artifacts`, and the caching arm's whole move cost is repaid by one request. Filtering before evaluation stays refused. **A second route removes the artifact count from the request path**: a single-valued attribute predicate partitions the corpus, so one masked histogram over the column it names answers every artifact at once, flat in the layer size — **175 ms against 462 ms** at 10⁶ artifacts over 10⁷ points. The column is `attrs/`'s entity-space `ValueColumn`, not the render table. A crossover near 30 000 artifacts at that corpus size, no new storage, and nothing on the wire naming which route served. **The scale campaign then found that route is the general one** ([the memo](design/artifact-serving-at-scale.md), [the probe](../probes/2026-08-20-artifact-serving-scale/README.md)): what decides cost is row-space **locality** rather than membership source, so a **row-major** layout — a label per row where the layer partitions, a list per row where it overlaps — serves every scattered shape, and at 10⁹ points it is the only layout that fits at all, 4 GB against 78.5. **Three rulings, 2026-08-21**: no declared bound, and every build reports blocks per artifact ([0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)); nothing per token over the artifact population, containment coming from a build-time partition over terms ([0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)); the layout chosen automatically per (layer, level), pinnable per layer, re-evaluated at each fold ([0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)). **The per-request bound is withdrawn, not deferred.** **The adversarial review ran and is dispositioned** ([the record](evidence/memos/2026-08-21-artifact-serving-scale-review.md)): two fail-opens found in the unbuilt design and amended — the settled half of the candidate walk tested containment where it needed the mask, and the containment partition's deny correction is its acceptance test rather than a refinement — 0093 amended with the owner's row-major histogram exception, and two leak-register annotations approved (architecture **r49**). **The re-measurement has run** (2026-08-22, medians of three runs of three iterations on the corrected probe): the masked candidacy the old routes omitted is real and is now the dominant term of every figure — the whole-map cell at 10⁷ artifacts over 10⁸ points was 31.7 ms and is **553 ms**, of which candidacy is 549 — and what survives is the conclusion, the design running **10–25× ahead of the per-artifact loop** with its worst measured cell at **905 ms** on one core. **The cost inversion is fixed**: the design's dear cells are broad masks and its narrow principals run sub-millisecond, where the per-artifact loop's worst cell at 10⁶ artifacts is a 3.1% mask paying 4.4 s. **And the campaign's largest design finding is the expression census**: 32 distinct containment expressions under `annotations.md` §8.1's per-term authoring, against **one per artifact** for generating sets drawn across the demo corpus's real 54,794-signature distribution — so the partition's wide-viewport union route exists **only** for per-term-authored sets, and for drawn sets containment degrades to a viewport-bounded per-candidate evaluation whose wide zooms the build wave has to price. **The serving build is under way on `artifacts/serving-build`, two stages landed** (2026-08-22): the row form split at its cost seam (`ArtifactRecords` always built, `MembershipRows` route-dependent), and the **containment partition built** per (artifact, rank) — expressions interned at `u16` with checked promotion, the two faces behind one type, the deny correction live on the accept path, the partition gated to the builtin plugin and absent under any other, its durable form fold-written at a **per-level version coordinate the manifest now carries** (a restart adopts at exact equality and recomposes otherwise, proved in both directions; the fold's retirement seam closed by omitting moved levels from both lists) — then the **tile index and per-artifact extents integrated into serving**: the per-ordinal sweep replaced by the walk, every candidate paying a masked probe, holes and empty projections distinguished by sentinel, the `everywhere` set first-class, one snapshot per level, extents-only durable (`TSTI`, the node hierarchy derived in the validating pass), and a differential test pinning the walk to the sweep ordinal-for-ordinal across shapes, principals, overlays and a mid-sequence growth. Gates read **1,878** then **1,898 passed / 0 failed / 11 ignored**. ⊘ The proportional criterion's denominator stays open (its row-major answer is stage three's declared-size column); the two cache-cadence defects the build depended on are **fixed and merged** (2026-08-21) — one write invalidates one level in one store rather than every view's every layer, and the lineage is held per generation and warmed at the fold |
| **7** Scale and the write cycle under load | not started — **pulled forward ahead of runtime artifacts** (owner, 2026-08-21) | 10⁹ points carrying 10⁶ artifacts, with 10⁷ as targeted probes, serve inside budget for many concurrent principals, fold under load, and leave the write path correct at every interleaving | **the serving half is measured on the corrected probe** (2026-08-22; [the memo](design/artifact-serving-at-scale.md) §7, [the probe](../probes/2026-08-20-artifact-serving-scale/README.md)), medians of three runs of three iterations. The target's two axes are measured one at a time: **10⁷ artifacts over 10⁸ points runs 8.4–905 ms** across the whole grid of principals by zooms, and **10⁶ artifacts over 10⁹ points runs 0.75–209 ms**, against 11–26 s and 1.8–8.3 s for the per-artifact loop at the same cells. Both hold the one-core one-second budget, and flatness in corpus size holds at 10⁶ artifacts (132 → 131 ms full-map from 10⁸ to 10⁹ points, worst cell ×1.5 for ×10 corpus). ⊘ **The corner where both axes are large is a negative result on this box**: 10⁹ points with 10⁷ artifacts reached 45 GB resident with all 12 GB of swap exhausted and no phase progress for three hours, and was killed — its cost there is **modelled from the flatness at 10⁶ (~550–900 ms) and needs a larger machine than this 47 GB one to measure**. Only the **cut** is in the engine: 1 008 ms → 188 ms by flattening the per-node lineages, which is where the peak RSS of 1 078 → 470 MB belongs, and then → **3.05 ms** at 10⁷ by the downward walk; serving exactly what it served before, checked against the reference implementation over random trees. What this stage owes is everything else — the generator's artifact arm reaching disk, the campaign matrix with its 1/8/32/128 concurrency sweep, **serving during a fold** at 10⁹ (the named gap; the fold's artifact pass is measured unloaded only), the write-path interleaving battery, and the configuration-matrix, census and conformance fill. [The plan](evidence/memos/2026-08-21-artifact-scale-plan.md); three tracks in flight |
| **8** Runtime artifacts | not started | a set of ten shared across a clearance boundary shows seven, and the day-one bookmark survives a hundred edits | it gains two items recorded rather than built elsewhere: the **layout-forcing control verb**, which sets the override for the next fold rather than rewriting a live level ([decision 0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)), and the **HTTP growth route** the design mentions and which does not exist |
| **9** Filters, search and excluding | not started — carries the `excluding` complement's price (owner, 2026-08-20) | an invisible artifact and a nonexistent one cost the same | it keeps the **replica-carried artifact tiers**, which Stage 7 declines in favour of the closed-form generator: a census answers at sizes where no expectation can be stored, and real geometry earns its fixtures here instead |

**The remaining stages are not monoliths, and the seams in them are deliberate.** The scale campaign
decomposes Stage 7 into tracks that run beside each other
([the plan](evidence/memos/2026-08-21-artifact-scale-plan.md)), and the first wave of them is
**landed and merged** (2026-08-21): the two cache-cadence defects (`artifacts/cache-cadence` — a
level's version rather than the store's, and the lineage held per generation), the write-path
interleaving battery (`artifacts/interleavings` — eleven constructed races), the generator's
artifact arm reaching disk (`artifacts/generator` — partition, boundary and treed arms with a
closed-form census, ~10⁶ terms reachable), and the probe corrections the review required
(`artifacts/probe-fix`). The merges were conflict-free and the integrated gate reads **1 855
passed, 0 failed, 11 ignored** across 158 binaries, clippy, layers, clients and links clean. The
interleaving battery landed **before** the serving build moves the write paths, which was the
constraint that ordered the wave. The serving build itself is in flight on `artifacts/serving-build`
— stages one and two landed 2026-08-22 (the Stage 6 row carries them); the row-major layouts, the
selection surface and the predicate routes are the chain's remainder. The serving path itself stays
one serialised chain: the review, then the cadence fixes, then the structures in the order §4.2 →
§4.4 → §5. *(This file previously said that with Stage 5 closed nothing remaining could run beside
anything else. That was true of Stages 6, 7 and 8 taken whole.)*

## 1. What decides the shape of this plan

Two facts do most of the sequencing work, and neither is about clusters.

**One decision cannot be taken later.** Entity IDs are allocated `(signature, source_id)`, and the
campaign measured `(signature, morton)` as **4.08× smaller** on the artifact disk form with term
postings **byte-identical** (M3, *measured*). It cannot be retrofitted under **I9**, so it is taken
before the first build that writes an artifact or not at all. It also renumbers every entity in
every fixture, so it wants to be taken *once*, in the same rebuild as the full-schema 2.4M corpus
[#88] is already committed to.

**All three designs are normative**, as of 2026-08-16. The Stage 0 review's findings are
dispositioned by decisions 0074–0083, the register carries their rows, and the two amendments the
rulings owed the normative architecture are performed — §7.5's descent and §7.7's ladder, with
§8.4's second threshold withdrawn alongside them (architecture r43). The roadmap's gate on this work
is discharged. **Five items stay open inside those normative documents**, marked ⊘ at their sites and
allocated to the stages that need them: search's containment gate and the filter axis (Stage 9),
membership packaging (Stage 2), the proportional criterion's denominator for predicate membership
(Stage 6), and the edit pass (Stage 8). None of them touches the spine, which is why promoting
before they close was the cheaper order — the alternative was holding an implementer on a question
about search.

Everything else is ordinary sequencing: a layer must exist before an artifact, an artifact before
its content, content before the events that can invalidate it.

**What already exists and is reused unchanged** — entity IDs, the deny lane, the WAL, the overlay
and both removal rules; the record blob (where supplied content lives); row space, the permutation
and the fold; the composed mask and `and_cardinality`; the filter tree and the token index. **What
does not exist** — anything artifact-shaped at all, and the WAL'd registry pattern the layer
lifecycle is specified against: `views-and-multi-table.md` §3 is a *design*, not code, so the layer
registry is the first implementation of that shape rather than a reuse of it, and views will
inherit it.

## 2. Stage 0 — what must be settled, and what it blocks

No code, and it is finished. The adversarial review is run, the ruling pass is made (decisions
0074–0083), the register rows are written and the two architecture amendments are performed.

| What | State | Blocks | Note |
|---|---|---|---|
| **Independent review** of the model and the representation | ✔ **run 2026-08-15** — three lenses, [the record](evidence/memos/2026-08-15-artifact-design-review.md) | — | the model's core survived all three; its *derived* rules did not |
| **Review ruling 1** — where artifact entity IDs come from | ✔ **ruled** — [decision 0074](decisions/0074-row-less-entities-are-allocated-downward.md) | Stages 1–4 | row-less entities allocate downward from the top; the repairs from the review's other findings are made |
| **Review ruling 2** — the masked count as an existence criterion | ✔ **ruled** — [decision 0075](decisions/0075-the-masked-count-is-an-existence-criterion.md) | Stages 1–2 | it never suppressed a count: it decides whether the artifact is served. Declared per layer, absolute or proportional, no default, **independent of the own-terms flag** |
| **Review ruling 3** — does a label's existence follow its content? | ✔ **ruled** — [decision 0076](decisions/0076-an-artifact-is-served-whole-or-not-at-all.md) | Stage 3 | wider than asked: **no levels of restriction within one artifact**, beyond ranked contents. C3's question evaporates; the model's degrade-to-derived is deleted |
| **Review ruling 4** — the artifact **edit** mechanism | **deferred to its own design pass** *(owner, 2026-08-15)* | Stage 8 only | not load-bearing: publishing and republishing artifacts needs no edit route. Two consequences, both stated rather than discovered — the emergency path becomes *suppress, then republish* (slower, not weaker), and runtime selections, whose whole lifecycle is editing, wait for it |
| Where supplied content **lives** | ✔ **ruled** — [decision 0077](decisions/0077-supplied-content-lives-in-the-record-blob.md) | Stage 3 | the record blob, at the artifact's entity. Its addressing is rank in the blob's **own** has-row bitmap, independent of row space, so an artifact having no row does not bear on it |
| **Does the attachment term inherit the target's criterion?** | ✔ **reversed 2026-08-19** — [decision 0089](decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md) supersedes [0086](decisions/0086-the-attachment-term-does-not-inherit-the-targets-criterion.md) | Stage 4 | it does, and by the whole target `verdict` rather than by the threshold alone: a dependent is served only where the artifact it depends on is served, and deleted when it is deleted, neither configurable. 0086 had declined exactly this and named the asymmetry it left — a viewer too sparse to be shown a cluster was still shown the label written about it — which is now closed at the price 0086 disputed, one masked count per attached artifact per request. 0086 stands unedited as the record of why the case first went the other way |
| **Review ruling 5** — how search gates on containment | open | Stage 9 | the term-signature conjunction is the candidate shape; the route stays withdrawn until ruled, which costs nothing before Stage 9 |
| **A hierarchy lives in its edges; levels are resolutions** | ✔ **ruled** — [decision 0082](decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md) | Stage 5 | a condensed tree is unbalanced, so a level number says nothing about lineage. A treed layer declares **no levels**; levels stay for balanced semantic resolutions and for stacked independent analyses, and the two are independent declarations. Rollup then falls out of per-artifact testing — ⊘ under an **absolute** criterion only, since a ratio does not shrink downward |
| **The frontier is a request-time budget** | ✔ **ruled** — [decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md) | Stages 2, 5 | levels had been bounding the response quietly; with the tree in edges a viewport intersects a root and every passing descendant. The cut's depth becomes a request parameter beside the mark budget, met by serving ancestors and never by sampling. **A budget is not a disclosure control** — every artifact it returns passed its own test — which §8.4's maximum depth was, and the two occupy the same place in a request |
| Implement [0072](decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md) — slot reuse | **deferred, deliberately** *(owner, 2026-08-15)* | nothing here | not a dependency: artifacts need identity, the deny lane and the opaque identifier, none of which need reuse. Deferring **removes** the recycled-slot fail-open rather than carrying it; the membership-reconciliation clause ships with it whenever it lands |
| **The signature-sort tiebreak** (rep §12) | ✔ **ruled 2026-08-15** — [decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md) | Stage 1 | taken: allocation becomes `(signature, morton_code, source_id)`. Free in the format and *measured* so; what it costs in the build's batch plan is named there and sized in Stage 1 |
| The descent change — per-artifact test replacing §7.5's tree walk | ✔ **ruled** — [decision 0080](decisions/0080-the-frontier-is-a-per-artifact-test.md) | Stage 5 | dropped. Non-covering hierarchies make the walk ill-defined; the rollup promise weakens and the caller supplies a covering top level if they want it. ✔ §7.5 carries the replacement (r43) |
| The label ladder reduced to guidance | ✔ **ruled** — [decision 0078](decisions/0078-the-service-takes-no-opinion-on-which-variation.md) | Stage 3 | the service resolves a caller-supplied ordering and chooses nothing. **A variation is a general artifact property**, not a label one |
| The three gate modes | ✔ **ruled** — [decision 0079](decisions/0079-the-gate-is-one-flag-not-three-modes.md) | Stages 1–2 | they were a two-by-two in three names. One flag — does the artifact carry its own terms — beside the independent criterion, which also stops a schema word disabling a disclosure control |
| The suppression-carry refusal | ✔ **withdrawn** — [decision 0081](decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md) | — | the premise was wrong: a refresh need not mint identities. An **edit** keeps them and suppressions survive natively; a **replacement** ends them and nothing carries, correctly. A report replaces the refusal; the key is optional again |
| Membership as a filter (rep §7) | open | Stage 9 | a disclosure question, not a cost one |
| Appendix C edits | ✔ **done** — architecture r43 | Stages 2–4 | **C1** annotated twice: a criterion bounds a grouping's existence and shape and **never its count**, which §7.1 and §7.3 already serve exactly, so a compact artifact's masked count is recoverable by summing the underlay whatever it declares; and where several layers cover the same points, the most permissive declaration governs what is recoverable about all of them. **C27** — the artifact's own-label declaration, C23-shaped; it follows `artifact_visibility`'s `field` since the two axes landed (0088, architecture r46). **C28** — the caller's membership requirement on supplied content, C12-shaped, `High if mis-declared`; it follows `require_member_visibility`. **C17** — an artifact identifier probes the same channel and stays inside the same bound. C7's disposition was already written (r42) |
| `derived-artifact-gating.md` — retired | ✔ **deleted** 2026-08-15 | — | its taxonomy is superseded; what existed nowhere else — the point-scale edge argument, the edge gate's form, the induced-subgraph sampling problem — is carried in the model (§5, §11), and the roadmap names the three successors |

**One design item is owed and is not a ruling.** The entity budget under repeated replacement —
a 10⁷-artifact layer mints 10⁷ IDs per wholesale replacement against a `u32` space (write-cycle §9; the burn is replacement's, not the model's — decision 0081), with
[decision 0072](decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md) as the likely
answer since a dropped layer's slots return at the fold. It is due before Stage 8.

**The second item on this list is withdrawn.** The **per-request bound on predicate evaluation**
(rep §2.0) was owed here, due before Stage 6, on the premise that without it a nationwide boundary
level is seconds per query. That premise held while a predicate layer's only serving form was one
masked scan per artifact; the row-major layouts remove it for every layer that partitions, and what
is left — scattered *and* overlapping *and* numerous — is reported at the build rather than bounded
at the request ([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)).
No bound machinery is owed by any stage.

## 3. The stages

Each stage ends in something that can be demonstrated against a real corpus and checked by a test
that fails if the stage is removed. Data each stage needs is named here and specified in §5.

### Stage 1 — The spine: entity allocation and the layer registry

**Capability:** a layer exists, is reachable or not by gate, can be suppressed immediately and
dropped permanently — with no artifacts in it at all.

- [Decision 0073](decisions/0073-entity-ties-are-ordered-by-morton-code.md)'s tiebreak in the
  build's comparator, in **both** build paths; fixtures rebuilt; postings verified byte-identical
  and the permutation's monotone runs verified at ~44 against ~1 (*measured* prediction from
  M3/M7). The decision names two record layouts for the signature sort and does not choose between
  them — that choice, and its effect on the feasible batch, is this stage's first measurement.
  Projection figures are re-baselined here, since gathered rows now arrive in sorted runs.
- Layer registry: manifest section plus WAL'd create/drop/tombstone, served as manifest + WAL
  overlay. One entity ID per layer, so layer suppression rides `/control/changes`.
- Reserved entity runs per level as a **list** of 2¹⁶-aligned blocks, never one block.
- `/v1/meta` carries the gate-filtered registry — identity, structure, zoom-to-level map, declared
  derived vocabulary, views, supplied-content kinds. **Never the artifact cardinality.**
- Layer reachability keyed on the layer version, with a live `verdict` check ahead of it.

**The check:** a layer is visible to one principal and, to another, indistinguishable — in outcome
*and in work* — from a name that was never registered; suppression takes effect at the ack for an
already-open session; a dropped name is refused on recreation; all of it survives WAL replay and
restart. **Data:** none new.

**Where the registry got to** (2026-08-16, `artifacts/stage-1`). All five bullets are built and the
gate is green: `cargo test --workspace` at 134 binaries, clippy at `-D warnings`, `check-layers` and
`check-doc-links` clean.

⊘ **One test does not survive the branch.**
`a_stalled_or_disconnected_stream_is_shed_and_the_gauge_returns_to_zero` (server `http` suite)
fails on **both** artifact branches (three runs on `stage-2`, one on `stage-1`) and passes on
`text/owed-work` — so it arrived with this stage rather than with the client work above it. What breaks is the
assertion that a stalled reader's held response must not read as complete; the gauge half of the
test passes, so the shed itself still happens. **Why is not established**: the assertion needs the
body to be large enough to fill the socket and channel buffers before the stall deadline, and the
tiebreak reorders what that fixture's request gathers, but nothing here measures the two response
sizes. It wants whoever owns the streaming test.

**The entity space grows from both ends now, and the reason is not identifier supply.** A flush's row
table, a merge's window and the fold's pre-flight all size themselves over entity *ranges*, so a
row-less run sitting between two point segments is dense waste in a resident array and budget in a
fold that can refuse for it — and under one monotone allocator that interleaving is the *normal*
case. Two regions make it unrepresentable. Exhaustion becomes the two marks meeting, which is the
true condition where a fixed ceiling per region would be a guess about the split.

**Row-less allocation is whole 2¹⁶-aligned blocks and nothing finer.** A level's membership then
sits inside whole Roaring containers; a level straddling a boundary pays for a partial container at
each end on every operation for ever. The top 65 535 ids are unusable by design, with a test saying
so, because "recovering" them would put the first block across a boundary. Layer entities come from
a block the registry holds — mixing widths in the allocator leaves the mark unaligned after every
single-entity allocation.

**Both marks are durable in both homes**, which is the recovery hazard
[decision 0074](decisions/0074-row-less-entities-are-allocated-downward.md) names as the part to get
right: rotation reclaims the `LayerCreate` records the mark is otherwise recovered from, so
`SEGMENTS-<n>.json` gains `entity_id_low_water`, `layers` and `layer_tombstones` — **required, not
defaulted**, because a default makes an absent mark and a *lost* mark the same value, and the
safe-looking default is the ceiling, which is exactly the value that reissues every live layer's
ids. Both names join `HONOURED_STATE` beside the code that acts on them. The fold carries all three
forward untouched: it rewrites the point region, and a layer has no rows to renumber.

**A registration is validate, allocate, append, sync, apply — and nothing is applied before the
record is durable**, which is the opposite of a deny's posture. A suppression is applied even when
its append fails, because leaving an accepted deny unapplied is a fail-open; a layer has no such
asymmetry, since one existing in memory and not in the log comes back from a restart as a free name
having already handed a caller an identifier for its entity.

**Two traps found, each of which would have shipped silently.**

*The admin plane's misdirection guard was written for one region.* It refused any entity at or above
the point mark, so every layer identifier this deployment could issue read as naming nothing — and
the symptom is not a range check, it is that suppressing a layer answers *no such thing*. The issued
space is two ranges; what names nothing is the gap between the marks. Verified load-bearing by
reverting it, which fails two of the six engine tests.

*`skip_serializing_if` is fatal under postcard,* which decodes positionally: omitting an absent
`Option` shortens the record and every field after it reads the wrong bytes, so a layer can replay
carrying a different gate. The rule is now absolute at the site, with a test that turns re-adding one
into a red build.

⊘ **The layer-entity cursor is deliberately not durable.** A `LayerCreate` records which entity a
layer took, not which block it came from nor how much was left, and resuming from `max + 1` is wrong
the moment a drop retires the highest-numbered layer. Reseeding costs at most one block per restart
out of 65 536; a durable cursor would buy back an id space nothing is short of.

**Where the tiebreak got to.** Landed in both build paths as a **16-byte sort record**, with the
batch plan's residency model widened to match — a model left at 12 would plan a batch the loop
cannot hold. The codes reach the sort through a mapped `morton-of-ordinal.u32`, filled by its own
pass over the points file, because an ordinal exists only once the source ids are sorted and the
first pass sees the file's order rather than the corpus's. **No existing test moved** — the suite
pinned signature *grouping* and never the order inside a group — so the new one asserts the
direction on both the pre-sort and the tie-refinement path.

**What the real 2.4M corpus says** (*measured* 2026-08-15, builds of the same corpus through the
shipped writer, warm cache, `categories-subclass`, **minimum of three runs**):

| | baseline | shipped |
|---|---:|---:|
| build wall time, 2.4M | 7.34 s | 8.35 s (+14%) |
| build wall time, 25M | 22.11 s | 25.11 s (+14%) |
| points-file passes | 3 | **3** |
| `postings.arrow`, `pairs.parquet`, `morton.u32` | | **byte-identical** |
| mean monotone run in `permutation.bin` | 2.00 | **49.19** |

**Read single runs as ±1 s at 2.4M and ±5 s at 25M**, and quote minima of three. An earlier revision
of this section carried **+49%** from one run — noise, withdrawn, along with the precise
`+0.33 s record / +3.04 s pass` split derived the same way.

**Two results correct the design.** M3's *"postings byte-identical"* now holds end to end rather
than in a re-derivation — and it is structural, not luck: a signature group occupies the same
contiguous entity range however its interior is ordered, so a term's postings cannot move. M7's
permutation claim was *"~1 → ~44"*; the measured pair is **2.00 → 49.19**, because the uncorrelated
baseline for a random permutation is 2, not 1. The direction and the destination hold; **the ratio
is 24.6×, not 44×**, and anything quoting the old baseline is quoting a mistake.

**Where the geometry read goes, settled at 25M** (*measured*, minimum of three warm runs; every
variant produces a **byte-identical** bundle, `created_at` aside):

| | 2.4M | 25M | |
|---|---:|---:|---|
| baseline, no tiebreak | 7.34 s | 22.11 s | |
| **a)** separate Morton read — four points passes | 8.84 s | 28.56 s | +29% |
| **b)** geometry read once, standalone permute | 8.20 s | **30.66 s** | +39% — *worse than (a)* |
| **c)** geometry read once, **fused into the assignment walk** | 8.35 s | **25.11 s** | **+14%** — shipped |

**2.4M cannot answer this question and nearly gave the wrong answer.** At that size the corpus fits
one batch, all three variants sit within a second of each other, and (b) looked like the winner. At
25M (b) is the worst of them. Anything decided from the small tier here would have gone into the
10⁹ rebuild backwards.

**The diagnosis that mattered was not the one this file first recorded.** (b) was written believing
the standalone permute added random access. It does not: the scatter into entity-ordered geometry
exists in *every* variant, including the baseline, where it sits inside the geometry scan's own join
callback — and at 25M that callback iterates ordinals ascending too, because one join chunk covers
the corpus. What (b) actually did was trade **parallel** work for **serial**: `scan_points` decodes
row groups on a worker pool, so the pass it removed was multi-threaded streaming, and the permute
that replaced it is a single-threaded traversal. That correction came from an independent
algorithmic pass over the stage, and it is what produced (c).

**(c) pays for the geometry move where the build is already paying.** The assignment walk visits
every item with both indices in hand, randomly writing `entity_of_ordinal` and randomly incrementing
a per-term counter; adding two gathers to a loop already stalling on memory costs far less than a
traversal of its own. Entity ids ascend with position there, so the geometry *writes* stream. The
ordinal-space arrays are released as soon as that walk ends — at 10⁹ that returns 8 GB of dirty
mapped pages before the band sweep and postings write start competing for cache.

⊘ **What survives is +14%, consistent across both tiers**, and it is the tiebreak itself: the wider
sort record, the extra comparator field, the ordinal-space arrays and the two gathers. ⊘ **And the
10⁹ ordering is modelled, not measured** — the argument that (a) cannot win there is that ~25 GB of
points will not stay in page cache beside the build's own scratch, so its fourth pass becomes real
I/O rather than a cached decode. That is the claim to check when a 10⁹ build is first run, and it is
the reason (a)'s 25M win over (b) was not taken as the answer.

### Stage 2 — One flat level, served with masked counts

**Capability:** a viewer sees the clusters their own visible set generates, with a count that is
never the cluster's size.

- ✔ Enumerated membership, entity space only, and the publication that gets it there: a batch is
  one WAL record and one fsync, ordinals are claimed densely from the level's cursor on the
  executor, and a level that outgrows its 65 536-entity reservation appends another block — carried
  in the record, because ordinals walk the runs in *allocation* order and a re-derived extension
  would renumber everything above it. `PUT /control/layers/{name}/artifacts` accepts members by
  external id or `tessera_id`, resolves them once at the boundary, and refuses the whole batch on an
  unresolvable one: a dropped member moves both the count a viewer is shown and the size the
  proportional criterion divides by.
- ✔ **Membership has a home outside the WAL: the packed extent** (owner-ruled 2026-08-16 — the
  packed file, read normally, over both the record store and a mapped form). One file per level per
  publication, addressed by dense ordinal, behind **one** manifest entry — which was the whole of
  §2.4's open question, the bytes having always been affordable and the packaging not. Written and
  fsynced before the manifest that names it; the log is released only once that manifest is durable.
  A truncated extent refuses rather than decoding to a shorter one, because a membership that came
  back short is an artifact with a low masked count for every viewer, which the criterion renders as
  *absent* with no error to notice.
- ⊘ **A node that has published artifacts no longer folds.** The fold publishes a new prefix and
  extent paths are prefix-relative, so carrying them forward names nothing and dropping them loses
  every membership silently. Rebuilding them is `annotation-representation.md` §5.0.3's **fold
  artifact pass** — Stage 4's first measurement, and not a copy, since the fold retires deleted
  entities that a carried membership would go on counting. Until then the fold refuses loudly and
  counts a `fold_failures`. The cost is stated rather than hidden: segment count and tombstone load
  grow on a node serving artifacts.
- ✔ Found in the doing: **the registry reached a manifest only via a flush.** A deny-lane
  publication carried the deny state and not the layers, which was harmless while the WAL held them
  and fatal beside an extent — at open a layer's reserved runs are what turn an ordinal into an
  entity, so the extents would have been skipped whole and every artifact served as absent. Both
  publication paths now carry it.
- ✔ Found in the doing: **both publication paths clone a stale manifest.** A side-manifest write does
  not swap the generation, so a second publication that extended its clone would drop the first's
  entries. The extent list is held as complete current state and assigned, which is the posture the
  deny list already takes for the same reason.
- ✔ The derived **row-space** operator, built member-wise and never range-wise, cached per
  `(view, layer, level)` and rebuilt when the prefix, the segments version or the store's own
  version moves. Replace-on-mismatch rather than an LRU: the key names the only generation a
  projection is valid for, so a stale entry has no value to keep warm.
- ✔ The visibility predicate in one place, overlay first: `verdict` → layer gate → own terms ∧
  existence criterion (decision 0079), enforced on the **live** count.
- ✔ The count is taken against the **composed** mask, and the type enforces it: the trait the
  predicate reads has exactly one implementor outside a test build, so a count cannot be taken
  against the pre-overlay projection — which strictly contains `M_auth` after any accepted delete.
- ✔ Viewport carries the artifacts whose rows intersect the tile ranges, with masked counts, as a
  frame of its own (kind 5) after the counts and before any point. Drill-down by `tessera_id`
  calls the same predicate — an artifact reachable by identifier but not by viewport would be two
  transcriptions of one rule. Ordinals never cross the wire, and neither does an unmasked size.
- ✔ Contracts work: the artifacts frame, the **artifact budget** request field (accepted, and inert
  on a flat layer — the only reduction the representation allows is structural, and a flat layer
  has no ancestors to cut to), and a **layer selector** beside it, which narrows and never widens.
  The budget is defined here rather than at Stage 5 because it is a wire shape, and adding a
  request field to a shipped frame later is the change this ordering exists to avoid
  ([decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md)).
- ⊘ **No artifact carries its own terms yet**, so a layer whose `artifact_visibility` names a
  field serves nothing on either route. Fail-closed and deliberate: the per-artifact label arrives with content
  at Stage 3, and admitting an unlabelled artifact would make a missing declaration a grant to
  everyone.
- ✔ All three frame decoders — Rust, TypeScript, the Python oracle — know the artifacts frame, and
  `artifacts` is a compared surface in the conformance canonical form, so a determinism break in
  the artifact channel cannot pass every comparison in the suite. ✔ Drill-down is routed at
  `POST /v1/artifacts/{tessera_id}`, one `404` for every withheld case.
- ✔ **The client integration**, briefed in [the client handover](artifact-client-handover.md) and
  landed: `/v1/meta`'s layer list, the `layers` and `artifact_budget` request fields and the
  drill-down verb in `@tessera/client`; a layer control, cluster marks carrying their masked counts
  and a click-through in the viewer. The annotation channel issues its **own** request (`k = 0`,
  one named layer) rather than reading artifacts off the point path's responses, because the
  replica elides tiles it already holds and an elided tile carries no artifacts — clusters would
  have thinned out as the cache warmed. `scripts/publish-clusters.mjs` registers a layer and
  publishes a k-means clustering of the corpus's own points; there is no artifact geometry on the
  wire, so it writes centroids to a sidecar the viewer joins **by served artifact only**.

**The check:** on the real 2.4M clustering, a broad principal and a one-term principal receive
different counts for the same cluster, neither equal to its declared size; clusters below the criterion
are **absent**, not refused, and the response cannot distinguish them from clusters that never
existed. The conformance oracle recomputes every count from the same membership and the same mask
independently. **Data:** `clusters/hdbscan-2026-08` at 2.4M (§5.2), the seeded generator's artifact
arm at 10⁴ (§5.1).

**Met on the map, 2026-08-16**, on the 2.4M demo bundle with a 24-cluster k-means published over
156,828 of its own points. One cluster (`c-0013`, 11,008 members declared) across the five measured
principals:

| principal | visible items | clusters served | `c-0013` |
|---|---|---|---|
| narrow — term 14 | 243 | 5 of 24 | 4 |
| sparse — 1.9% | 35,138 | 17 of 24 | 485 |
| medium — 19% | 360,239 | 24 of 24 | 1,962 |
| heavy — 50% | 929,811 | 24 of 24 | 4,138 |
| full — top 4096 terms | 1,856,276 | 24 of 24 | 8,380 |

No principal's count equals the declared size, because the clustering was published from a
principal broader than any the viewer offers — the top 16,384 ranked terms. Under
`require_member_visibility = { count = 1000 }` over the *same* membership the five are served 0, 0, 8, 20 and
24 clusters: presence itself moving with the mask, and the response saying nothing about why.
Reproduced by `clients/ts/viewer/smoke-artifacts.mjs`, which fails if the counts stop moving with
the principal.

✔ **The deployment-wide `min_visible_members` key is deleted, not wired**
([decision 0085](decisions/0085-the-existence-criterion-has-no-deployment-wide-form.md), 2026-08-16)
— the reconciliation architecture r43 deferred to this stage. A deployment default would make an
undeclared criterion mean *inherit this floor* where [decision 0084](decisions/0084-an-undeclared-criterion-declares-no-test.md)
rules it means *no test*, and no layer could then decline it; the proportional form has no
deployment-wide parameter to default in any case. `[disclosure]` stays required and holds
`token_max_lifetime` alone, and the startup obligation the key carried — state your disclosure
parameters, do not inherit them — is discharged at layer registration, which has no default either.
§7.5's threshold stops being ⊘ specified-and-unimplemented and becomes a control that runs
(architecture **r45**).

✔ **A zero masked count is served by the drill-down where the viewport withholds it**, on a layer
that declares no criterion — and that is **ruled correct**
([decision 0084](decisions/0084-an-undeclared-criterion-declares-no-test.md), owner, 2026-08-16).
The viewport rule is *any member visible to this principal falls inside the requested tiles*, so a
cluster this principal can see none of never appears on the map; the identifier route applies the
existence predicate alone, which an artifact with a zero count passes when
`require_member_visibility` is `"none"`. A declaration of *no threshold* is a declaration, and the service adds no floor of its own:
one that the schema cannot express, applied on the service's initiative, would be a rule nobody
wrote and nobody could turn off. `require_member_visibility = { count = 1 }` expresses the floor exactly, for a
deployment that wants it. The cost is recorded rather than sheltered — C17's bound moves from *items the
principal already sees* to *the layer's gate*, and the decision carries the three properties that
bound it. No code changed.

### Stage 3 — Content: derived, supplied, and the containment test

**Done on `artifacts/stage-3`.** The traps this machinery carries were carried forward into
Stage 5, which is now closed; the Stage 3, 4 and 5 handovers are all retired.

**Capability:** a label is served only to a viewer who can see everything it was generated from.

- ✔ The declared derived vocabulary — count intrinsic, `centroid`/`box`/`hull` opt-in — under the
  closure rule that a derived property is a function of `membership ∩ M_auth` and nothing else. The
  rule is enforced by the type rather than by care: the visible rows come from the composed mask's
  own `visible_rows`, which is the only way to obtain them, so a property computed over full
  membership has no input to be computed from. ⊘ `extractive_terms` is **refused at registration**
  along with every other unknown name, because a layer registered with content nothing computes
  serves artifacts a client cannot tell from ones whose content was withheld.
  - The geometry travels in the **grid units** the points frame's `code` is built from, so a client
    draws it with arithmetic it already has and needs no quantisation extent. The hull is integer
    monotone-chain in `i128` — an `i64` cross product overflows on a hull spanning the map, which is
    the ordinary case rather than an edge one — so the vertex list is exact and identical across
    platforms, which is what lets the conformance oracle compare it byte for byte.
  - Found in the doing: the **count and the geometry must be taken over the same set**, and nothing
    was asserting it. `count_intersection` composes term by term and never materialises;
    `visible_rows` materialises. A test now pins their agreement under a suppression and a buffered
    arrival together — the two terms that make the composed mask differ from the projection at all.
  - The **wire shape reached `contracts.md`** in the same pass: the kind-5 frame had shipped at
    Stage 2 without a contracts entry, so §3.2 listed four frame kinds and the server emitted five.
- ✔ Supplied content in the record blob; containment `|G ∩ M_auth| == |G|` against the **composed**
  mask, cached nowhere, and costing the same on the pass and fail paths — one count over the whole
  set, no early exit, because a short-circuiting subset test returns sooner the *less* of the set a
  viewer holds and makes response time a function of how close they came. `G` is a Roaring set
  projected into row space beside the membership rather than the design's sorted entity array: the
  numerator and the denominator then come from one projection, and the array is an optimisation to
  take at a scale nothing here reaches.
  - **Artifact properties are ordinary entity-keyed properties** *(owner, 2026-08-16)*. Same store,
    same format, same reader as a document's blob-resident fields, in extents of their own on the
    same list — so the filter and search surfaces reach them by the route they already reach a
    document's when those land, rather than through a parallel stack. What does **not** transfer is
    the access rule, and that is the distinction `annotations.md` §7's withdrawal turns on: its
    *storage* half survived review, its *visibility* half was the fail-open. Sharing is safe because
    the two never share an entity — artifact ids descend from the ceiling, point ids ascend from
    zero — so which rule governs a row is a range check on its id.
  - ✔ **Layers, levels and their artifacts are definable at build time** *(owner, 2026-08-16;
    built)*. `tessera build` registers the corpus declaration's `[[layer]]` blocks into the
    manifest, and a layer's `source` and its `[layer.members]` source publish into them —
    memberships, ranked content, generating sets and attachment edges — so a bundle is served with its annotations already there and a
    10⁷-artifact level never rides the trickle path, where every batch is an fsync and the log is
    pinned until a manifest carries it.
    - **One implementation of the rules, not two.** The build calls the same registry, the same
      allocator and the same publication the control plane calls, and discards the WAL records they
      return because a build's durable output is its manifest. So a declaration refused online is
      refused at build with the same words, and ordinals and entities land where a registration
      would have put them — verified by a test that registers a layer online *after* opening a
      built bundle and finds no id reissued.
    - **Members are named by source id**, resolved through the build's own assignment as the pairs
      file's ids are; a `tessera_id` would name an entity space the build is still assigning. An id
      the build did not assign refuses the build rather than being dropped.
    - **Ordinals are a function of the artifacts, never of the file's row order** — publication is
      in `(layer, level, key)` order, and a key is required, because an ordinal is
      identity and an edge names its target by key.
    - The one refusal that has no build-plane meaning is publish-time validation against the deny
      lane: a bundle straight out of `tessera build` has no overlay, so no declared member can be
      deleted yet.
    - ✔ **A bundle built with artifacts folds** — it did not, from its first open, which was the
      build plane's sharpest cost when review 2026-08-16 found it: the fold's refusal keyed on
      published memberships being *there* rather than on their having arrived online, so the
      bundles this route exists for were exactly the ones that could never compact. Stage 4's
      artifact pass rewrites them instead, and it does not care which route wrote them.
  - Found in the doing: **a generating set can lose members on the way into row space.** The set is
    entity-space and permanent; row space holds only what this view has folded in, so a member
    awaiting a fold projects to nothing and drops silently out of the test — leaving a viewer
    contained in a *smaller* set than the caller wrote. The projected set now travels with the size
    it should have had, and one that lost members contains nobody.
- ✔ Ranked contents: one artifact, one identity, the first satisfied served entire — or no artifact
  at all (decisions 0076, 0078). The three outcomes are a type rather than an `Option`, because
  *this layer declares no content* and *you may not read this content* are different answers and
  collapsing them serves the second case with its description missing.
- ✔ **Supplied content crosses the boundary.** `PUT /control/layers/{name}/artifacts` takes a
  `content` list per artifact — ranked contents, each with its values and its `generated_from`
  set — and resolves the generating sets in the *same* pass as the members, by the same addressing:
  a `tessera_id` in durable state would be reinterpreted by the next key rotation, and a containment
  test over a set naming other documents than the caller wrote is a disclosure rather than a stale
  answer. The kind-5 frame carries a `content` list column and the drill-down a `content` field,
  through all three decoders. **A null is never *withheld*** — an artifact whose content a principal
  may not read is absent from the viewport and a `404` on the identifier route — so an empty list
  means *this layer declares none* and nothing else.

**The review of the first three commits, and what it changed** *(2026-08-16, two independent
lenses — disclosure and invariants; correctness and durability)*. The disclosure lens found the
predicate clean: the criterion, the gate, suppression, the count and the geometry all hold, both
serving routes agree, and no unmasked quantity or reason-for-absence reaches the wire. The
durability lens found one serious defect and three real ones, all now fixed:

- **A second publication silently un-named the first one's content extent** — the manifest a
  publication starts from is a clone of the *stale* generation's, so appending to it drops every
  earlier entry. Once the log is released, that file holds the only copy of its labels, and its
  artifacts come back with content that cannot be read and are withheld from every viewer with
  nothing reporting a fault. **This is the identical bug the membership list was given a held list
  to fix, reintroduced one line below the fix for it.** Artifact content extents now have their own
  manifest list, held complete in the executor and assigned rather than extended;
  `two_publications_of_content_both_survive_the_loss_of_the_whole_log` fails without the fix,
  losing exactly the first artifact.
- **The extent's two addressing files were neither fsynced nor digested.** `RecordBlobWriter::finish`
  syncs the blocks alone, and a torn directory or has-row bitmap refuses the **whole** record stack
  at open — every point's blob-resident field, not just the artifacts'. Both are synced here now.
  ⊘ They are still absent from `manifest.files`, so a failure is unattributable to a digest.
- **The field-tag overflow guard dropped a field where its comment claimed it dropped the row**,
  which made the in-memory and on-disk copies disagree: served in full before a restart, withheld
  after. The row is abandoned whole now.
- The fold's refusal on a corpus with no blob-resident column names the wrong cause once artifact
  content exists (⊘ Stage 4's fold pass owns it; the refusal is currently shadowed by the membership
  one, so nothing changes in practice). **The multi-partition case now refuses loudly**: an artifact
  belongs to no partition, so writing its content once per partition would give one record stack two
  layers with overlapping has-row bitmaps — a state the stack refuses outright, breaking every later
  coalesce over a window holding both. Which partition should own it, or how the rows should be
  split, is a layout question only a multi-partition bundle can answer and none exists; refusing is
  the honest placeholder. ⊘ The extent is still absent from `manifest.files`, so a torn one is
  unattributable to a digest.

**What the review confirms about the uniform-storage ruling.** A coalesce over an extent holding
both point and artifact rows composes correctly, and a fold over one loses nothing — because the
two id regions cannot overlap, so a mixed window is two disjoint ranges and the merge's
disjointness check passes rather than fires. That was the interaction the ruling could not be
checked against when it was made.
- ✔ **The attachment edge, and the term that is fail-open without it.** An artifact published as an
  attachment to another — a label on a cluster — is tested on its target's **disposition** (the
  overlay's `deleted > suppressed` composition, which is what `annotation-representation.md` §4
  means by *the target's `verdict`* — the same lookup the predicate's first branch performs) **and**
  on its target's gate, in the one predicate, so it holds on every route rather than on the ones
  that traverse the edge. Suppress a cluster and its labels stop serving in the viewport *and* on an
  identifier a viewer already holds; the same for a deletion, for the target layer's own
  suppression, and for a viewer who does not reach the target's layer at all.
- ✔ **The edge carries both of the member grain's meanings**
  ([decision 0089](decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md), owner
  ruling): a dependent is **deleted when its dependency is deleted**, and **served only where its
  dependency is served** — per artifact, neither configurable. The visibility half is the target's
  whole `verdict` rather than the three cheaper questions above, so a cluster withheld by its own
  criterion no longer has its label announcing it; that was 0086's accepted residue and 0089
  supersedes it, paying the target's masked count per attached artifact per request. The deletion
  half rides the deny lane — a record of its own in the same window as the deletion that caused it,
  retiring at the compaction fold that executes it (Rule F), never by a second removal route — and
  it is transitive, a label on a label going with both. **Every artifact of a layer that declares
  `depends_on` must declare an attachment into a declared layer**, refused at the build and at the
  control plane alike.
  - **The caller names a target by its key**, because an ordinal never crosses the boundary
    (C8) and a `tessera_id` in durable state would be reinterpreted by the next key rotation. What
    is stored is the resolved `(layer, level, ordinal, entity)`, which is what makes the extra term
    one `verdict` lookup rather than a walk through the registry.
  - **Two refusals at publication, both fail-closed.** A target that does not exist yet is refused
    rather than stored — an edge names a position in a dense level, so one written first would name
    whatever later landed there (§5.0.4) — and so is an edge into a layer the attaching layer did
    not declare in `depends_on`, which is what makes the layer-level refusal of a dangling
    replacement sound: a dependency nobody declared is one no replacement checks.
  - The record and the WAL both carry the edge, and both formats bump their version for it: an
    attachment lost on the way back from disk is a label serving over a suppressed cluster, which is
    the fail-open reappearing at a restart with nothing reporting a fault.

**The check:** the design's own worked example, reproduced on real data — a broad viewer and a
narrow viewer fail the *same* full-sample label for the same reason, and both satisfy its per-term
variant. Suppressing a cluster stops its labels serving on search and by held identifier, not only
on traversal. **Data:** `topics/ctfidf-2026-08` and `centroids/kmeans-2026-08` (§5.2).

**Met on the real corpus, 2026-08-16.** Twenty-four k-means clusters over 158,434 of the 2.4M
bundle's own points, and twenty-two labels attached to them, each carrying two ranked descriptions:
one generated from the whole cluster, one from the part of it a single term's principal can see.

| principal | visible items | clusters served | labels served | description served | `l-c-0001` masked count |
|---|---|---|---|---|---|
| narrow — term 14 | 243 | 5 of 24 | 0 | none | absent |
| per-term — term 46 | 15,188 | 22 | 22 | per-term | 9 |
| broad — term 79 | 181,900 | 21 | **0** | **none** | absent |
| half — terms 0–60 | 1,256,894 | 24 | 22 | per-term | 6,497 |
| whole corpus — 176 terms | 2,422,486 | 24 | 22 | whole cluster | 6,797 |

**The third row is the result.** That principal sees 7.5% of the corpus — twelve times more of it
than the row above — and is served **no label at all**, because what they can see is not what the
description was generated from. Containment is not a coverage fraction: what decides is *which*
documents. The rows either side of it are the design's worked example proper — two principals three
orders of magnitude apart in what they can see, served the *same* description because both hold the
term it was generated from, and each told a count of their own beside it (9 against 6,497). Only the
principal who can see the entire corpus is served the whole-cluster description, and no served label
is ever short: a viewer containing no content receives no artifact.

**And the label does not outlive its cluster.** Suppressing `c-0001` removes it and `l-c-0001`
from the viewport, and the **identifier route** — which traverses no edge, and is what a viewer
holding a label from a moment ago would use — answers `404` for the label as well. Lifting the
suppression restores both; the label's own entity was never touched.

Reproduced by `clients/ts/scripts/publish-clusters.mjs --labels … --label-term …` followed by
`clients/ts/scripts/check-labels.mjs`, which **exits non-zero** if any of those claims stops
holding — a table alone would print just as happily if the answers stopped depending on the
principal. Fixture note: bench bundles predating a manifest field refuse to open by design, so
`scripts/bench_build_fixtures.sh --scales 2422486` rebuilds the 2.4M corpus first (ten seconds).

### Stage 4 — The write cycle

**Done on `artifacts/stage-4`, merged to `main` 2026-08-17.** What it carries forward went into
Stage 5, which is now closed; the Stage 4 handover is retired.

**Capability:** ingest, delete, suppress and the fold leave every artifact correct, and the fold
does not resurrect a withheld label.

- ✔ The row operator's arms — and there is **one**, not three. The plan called for union at flush,
  rebase over the merged span at a merge, and rebuild at the fold, with the merge arm named as the
  one a reader leaves out. The write cycle's base-row rule (§4.1) removes the state the other two
  would operate on: the form holds base rows only, so a flush appends rows it does not hold and a
  merge renumbers rows it does not hold. The fold rebuilds it, inline, and nothing else has to.
- ✔ The fold's artifact pass, in the cheaper of the two constructions (§6 measures which), plus
  **Rule F's artifact arm** — membership file, `artifacts.arrow` slot and every edge naming a
  deleted artifact dropped *before* the overlay entry retires.
- ✔ The fold's report sweep: one `and_cardinality` per artifact against the deletions the fold
  executes, written before the flip and outside the prefix (a fold reclaims the prefix it
  supersedes), with a report that cannot be written discarding the fold. ⊘ The feed is still a file
  and an accessor rather than a subscription.
- ✔ The strict/permissive declaration, strict by default, executed by the fold — and an artifact
  whose last content is withdrawn is **absent** rather than served without its description.
  Publish-time validation beside it, reaching generating sets as well as memberships.

**The check:** the correctness suite's stage battery extended to the artifact surface — recorded
after every one of the eight stages, judged by the same three mechanisms. The regression test this
stage exists for: delete a member of a label's generating set, watch the label vanish at the ack,
run a fold, and **it stays gone**. **Data:** the seeded generator at 10⁶–10⁸ (§5.1) — real data
cannot carry this, because the check is "nothing is missing or extra" over all *n*.

✔ The regression test is `clients/ts/scripts/write-cycle-demo.mjs`, which drives a live deployment
over the control and viewer planes rather than the engine's own types, and passes on the 2.4M
bundle. It publishes the pair it will damage: a generating set never crosses the trust boundary, so
a driver that followed someone else's labels would have to delete documents at random until one
landed in a sample. The fold's report is the corroboration — the deleted document belonged to
clusters in five other published layers, and the report named each of them and the content each
one lost. ⊘ The battery itself does not exist, so the census (`artifact_census.rs`) stands alone
rather than as one of its rows.

### Stage 5 — Trees, levels and the cut

**Done on `artifacts/stage-4` 2026-08-18.** The handover is retired, and with it the Stage 3 and
Stage 4 handovers it superseded.

**Capability:** a layer's lineage lives in its edges, its levels are declared resolutions, and a
viewport returns a cut through the tree that fits what the client can draw.

- ✔ Parent/child edges as the hierarchy, with per-artifact criterion testing and no walk
  (decision 0080). The parent direction is durable on the artifact record and the child direction is
  built per level at serve time, so **no deletion has to keep two copies of one fact agreeing**.
- ✔ **A layer's edges all run within a level or all run between them, and the two are used for
  different things** ([decision 0087](decisions/0087-cross-level-edges-are-information-not-rollup.md)).
  `nested` is the clustering case, lineage entirely in the edges at level 0. `tiered` — the
  declaration value that was missing, and the reason the third shape could not be expressed —
  is a levelled layer whose edges run from a strictly coarser level to a finer one; administrative
  boundaries are its motivating example and a subject taxonomy is the first one published. Which
  shape a layer has follows from its declaration and is never inferred from its edges: an edge on a
  layer declaring no lineage is refused, one running against the levels is refused, and a parent key
  resolving in two coarser levels is refused rather than settled by search order.
- ✔ The **request-time artifact budget** (decision 0083), in the shape of the mark budget a viewport
  already carries, met by climbing to a **passing** ancestor and never by sampling — the defect an
  integration test caught, where a suppressed root blanked its children's regions. A budget takes
  nothing on a tiered layer, exactly as on a flat one: there is no depth to trade, because the
  resolution is the client's choice of level. ⊘ One depth for the whole tree is what this stage
  built; a budget resolving to different depths in different branches is the honest general case and
  is unspecified.
- ✔ Display pruning as a declared policy per layer (`prune_children`), threaded through to the cut
  rather than assumed — it was hard-coded to prune for one round, which is what made a parent link
  look as though it would mostly be null.
- ✔ Build-time containment verification that **reports** violating edges rather than deciding
  anything, plus a coverage report naming every split that keeps members none of its children hold.
- ✔ **A level is served partially and never withheld because part of it is suppressed.**
- ✔ **The structure reaches the client.** Each served artifact carries the identifier of its parent
  where that parent is in the same response, on the artifacts frame (contracts §3.2) under leak
  register **C29**. A parent that exists but was withheld reads as **null, identically to a root** —
  distinguishing them would disclose that a coarser artifact exists which the viewer may not see.
  The viewer indents each artifact under what contains it, names that parent in the drill-down, and
  draws the links on the map, lighting a whole subtree when one is opened. Rendering the structure
  is what the between-levels edges are for; an earlier draft answered containment as a *predicate*
  the client would ask about pairs, and that is declined — a client drawing two hundred features
  would issue forty thousand calls to reconstruct a tree it should have been handed.

**The check** had three parts, and the middle one is the reason this stage was not just plumbing.
*The non-covering case on real data:* a principal holding only the term covering a parent's stray
members is served that parent alone, masked count exactly the 687 members they can see, none of its
children, at every budget — and the build's report named the split in advance
([the probe](../probes/2026-08-18-condensed-tree/README.md)). **124 of the tree's 131 splits are
non-covering**, so that case is the majority rather than the edge. *The criterion's two forms:*
under `require_member_visibility = { count = n }` a passing child never sits beneath a failing
parent, and under `{ fraction = p }` one does — proved rather than assumed, the first version of that test having passed vacuously with
the parent covered by the frontier rather than failing its bar. *The budget:* a cut at one depth and
a cut at a deeper one agree on every artifact both return, and neither reveals an artifact that
failed its own test.

The cut is measured ([`artifact_cut_cost`](../crates/tessera-bench/src/bin/artifact_cut_cost.rs)) at
0.6 ms per ten thousand visible artifacts — **and at ten million, on 2026-08-21, 1 008 ms falling to
3 ms**: one depth interval per servable node in place of a lineage per frontier node, a walk down
from the roots in place of a sweep across the level, and the lineage held per generation rather than
rebuilt per request. On a fixture whose tree is a real hierarchy the cut is **0.3–11.4 ms** at every
principal and zoom measured, and what bounds such a layer is the masked count of a coarse node rather
than the cut at all ([the memo](design/artifact-serving-at-scale.md) §7.3) — after ordinal-indexed side tables and a memoised depth
replaced tree lookups. The corpus is re-derivable end to end from
[the notebook](../notebooks/README.md), which publishes all three shapes over the same points —
k-means flat, HDBSCAN's condensed tree nested, and arXiv's own classification tiered across two
levels. The last of those is also the **covering** counterpart to the first: every paper's primary
category sits in exactly one archive, so an archive is exactly the union of its classes, and its
32 splits lose nothing where HDBSCAN's 124 do.

**The owed tail is built** (2026-08-20; ruled by the owner the same day): a dependent is dropped
when the cut drops what it depends on.
[Decision 0089](decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)
makes a label visible exactly when the artifact it attaches to passes its own test, which
`dependency_served` reads by calling the same `verdict` every serving route calls. The cut then runs
*after* the verdicts and removes artifacts that passed — so a request carrying an `artifact_budget`
over a treed layer, alongside a layer depending on it, is answered with labels describing clusters
that response does not contain.

**The drop happens on the server and the attachment never reaches the wire.** The alternative was to
publish the attachment identifier so a client could drop those labels itself, and it is declined for
the reason `parent_id` carries a null rather than a withheld parent's name: handing over the
identifier names an artifact that is not in the response. A client that is never told the
relationship cannot notice what is missing from it.

This does not make the budget a disclosure control ([decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md)
stands). The pass can only ever remove, and everything it removes already passed its own test — it
decides what is *drawn*, and a label describing something not drawn is not drawn either.

It rides the pass that was already there: `serve_artifacts` holds the whole response before
resolving parents, in a `served_at` map keyed by `(layer, level, ordinal)` — exactly the triple an
attachment carries — and the drop runs **before** the parents resolve, so a dependent that goes
takes its own name out of that map and cannot be named as anything's parent. **The trap is the
request that names the dependent layer alone**, where a naive lookup finds no target and drops every
label; refusing a legitimate call is outside the disclosure surface, so the condition is that the
target's layer is *in this request* and its target is not in the response. One response never
contradicts itself; a request for labels alone is unchanged. **Chains cascade** — a note on a label
goes when the label goes — walked as a worklist over the edges the response holds rather than by
rescanning it per drop.

Three tests, in `artifact_hierarchy.rs` where the cut's own are. The headline plants one label at
each of three depths of the tree the budget tests already use, and asserts that each of the three
budgets serves exactly the one label whose subject that cut serves — so the cluster half of every
assertion is the cut's own expected frontier, unchanged. The second is the trap: the same fixture,
a budget of one, and a request naming only the label layer, which keeps all three. The third is the
cascade. Deleting the drop turns the first and third red and leaves the second green, which is
what the second is for.

**A target outside the viewport falls under the same rule** and is dropped with the cut ones. It is
rare by construction — a label's members are the documents it was drawn from, so a label inside the
viewport almost always has its cluster inside it too — and separating the two would mean carrying a
reason per absent candidate through a pass that deliberately collapses reasons. Recorded because it
is the one behaviour here that is not the budget.

### The configuration surface — the rework that ran beside Stage 5

**Stages 1–8 done on `artifacts/stage-4`, 2026-08-18/19** (`ead7e90` and `c3a595a`…`180e6b6`),
against the plan in
[`evidence/memos/2026-08-18-configuration-surface-plan.md`](evidence/memos/2026-08-18-configuration-surface-plan.md)
and the design in [`design/configuration.md`](design/configuration.md), ruled by
[decision 0088](decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md). Stage **9**
is the tail — the notebook, and the citations across the corpus — and is the only part outstanding.

**A build is now `tessera build` with no flags at all.** One document declares the corpus, its
views, its vocabularies, its attributes and its layers; `tessera.toml` names it; every `source` is a
path relative to the declaration and `--file KEY=PATH` overrides one. `--extent`, `--points`,
`--pairs`, `--values`, `--artifacts`, `--artifact-members`, `--schema`, `--layers`, `schema.toml` as
a fixed name and `layers.toml` are all gone, and every retired key is **refused** by the
unknown-field rule rather than aliased (decision 0048): a stale file is told so instead of read
wrong.

- ✔ **Two axes and only two.** `visibility` asks which access label the viewer must hold;
  `require_member_visibility` asks how much of the object's own membership they must already see.
  That retires `listing`, `gate`/`ungated`, `artifacts_carry_own`, `visible_when` and supplied
  content's `corpus_derived` — three spellings of the first question and three settings of the
  second. `public` is a reserved access label interned at term `0` and satisfied by every principal
  **inside the trust boundary**, not by grant and not in the plugin.
- ✔ **Points carry their own labels**, read from a field of the view's own source — a list, or a
  plain string — beside the exploded relation, which is unchanged. Filling never overrides, a null
  and an empty list both mean *no terms and so no principal*, and the linear and streaming builds
  are proved byte-identical on a fixture whose lexicographic and first-appearance orders disagree.
- ✔ **The plugin takes a term list.** The build had joined an item's terms with commas for
  `builtin:passthrough` to split apart, so a term written `ir:analyst,ir:legal` arrived as two
  grants; `terms_of_labels` is required rather than defaulted, and `Engine::open` now enforces the
  manifest's `data_plugin_hash` against the serving plugin — a check the corpus had claimed for some
  time and nothing performed.
- ✔ **One row per artifact**, from a layer's own `source` or an inline `artifacts` list, never both;
  `contents` is the ranked list and its index is the **rank**; membership may be spelled by
  `excluding` for a set that is nearly the whole corpus, complemented once in the build and never at
  request time. `stable_key` is `key` and the membership row's scalar `member` is `entity`.
- ✔ **`[layer.labels]` expands to a `[[layer]]` before anything compiles**, so the sugar meets every
  refusal a hand-written layer meets and the two build a byte-identical bundle. It supplies
  mechanism and **no disclosure control**: both member requirements and the artifact gate stay the
  caller's, required and undefaulted. `membership` is a table, so attribute membership names the
  field that carries it.
- ✔ **A dependency edge carries deletion and visibility**
  ([decision 0089](decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)) — recorded
  at Stage 4 above, taken during this rework, and reversing 0086.
- ✔ **The build says where the data sits inside the frame**, per view and before any work: the
  frame, the data's own bounds, how many points land on the frame's edge, and what proportion keep a
  cell of their own. Past half the points clamped it **refuses**, with no override — a frame that
  misplaces the majority of a corpus is not that corpus's frame — and a tenth of the corpus sharing
  cells warns. The threshold's limit is stated rather than overclaimed: at 10⁹ points concentrated
  in a hundredth of the frame a well-fitted build would warn too.
- ✔ **`tessera check`** parses the declaration and opens only Parquet footers, never a row, and
  collects every finding rather than stopping at the first, which is what makes it a CI verb.
  `--payloads` emits the control-plane bodies, closing the gap where a declare-but-never-build
  deployment authored every layer twice. **`reports/disclosure.json`** joins `containment.json`:
  every layer's gate, member requirement, dependencies and content, diffable by construction.
- ⊘ **Stage 9 outstanding** — the notebook emits one config and one source per layer, and the term
  dictionary becomes a real build output. The corpus citations are swept.

### Stage 6 — Predicate membership and the serving layouts

**Capability:** a boundary or a tagged set behaves as a layer, with membership derived per request
and never stale — and every layer is served by the layout its own shape calls for.

- Spatial predicate: the shape only, decomposed to Morton ranges, counted by `range_cardinality`.
- Attribute predicate: no new storage — the existing value column and postings.
- **The serving layouts and the surface that picks between them**: the containment partition, the
  hierarchical row-range index and extents, and the row-major label and list forms, under
  [decision 0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)'s
  automatic choice with its per-layer override. The design is
  [the scale memo](design/artifact-serving-at-scale.md) and
  [the selection surface](evidence/memos/2026-08-21-artifact-layout-selection.md); the order of work
  is [the campaign plan](evidence/memos/2026-08-21-artifact-scale-plan.md).
- ⊘ The proportional criterion's denominator, which this stage is where it bites: *"the points inside
  this shape"* declares no member set and its size changes at every write. Until it is ruled a
  predicate layer may declare an absolute criterion or none.

**This stage opened with a cost discussion and not with code** (owner, 2026-08-20), and the
discussion is held and measured
([the probe](../crates/tessera-bench/src/bin/predicate_membership_cost.rs)).

**A predicate layer's row form is cached and rebuilt at a generation move**, exactly as an
enumerated layer's is, so its per-request cost is Stage 2's and the predicate is invisible to the
serving path. The alternative — deriving per request by crossing the posting lists into row space,
which is what the filter surface's row route does for a leaf — is **7 900×** more expensive per
request at the demo corpus's 263 artifacts over the whole map: 6.9 seconds against 0.9 ms. A
crossing's cost is `rows × artifacts` where a cached count's is `containers × artifacts`; one
crossing does serve every set (decision 0062's rule holds), but each set still costs a probe per row
and a layer is exactly a collection of sets, so the ~5% crossover that makes the row route right for
a single leaf never arrives. **The caching arm's whole generation-move cost is repaid by one
request** — 15 ms at 263 artifacts — which is why the arithmetic did not need doing. Projecting per
request rather than crossing is the honest third shape and costs that same 15 ms *per request*; it
is the option to reach for only if the invalidation rule turns out to be the hard part.

**Filtering before evaluation is refused**, and the measurement is why it need not be argued again:
anything that skips evaluation on geometry is a disclosure decision taken on a stamp, the shape
[decision 0041](decisions/0041-pins-become-a-staleness-stamp.md) already refused for pins, and the
route that would have justified it is three orders of magnitude the wrong side of the one needing no
such filter. `membership = { attribute = … }` is declared and unbuilt today, which is the
fail-closed state to start from.

**What the discussion was called for remains true**, and is why it was held first: a predicate
membership is answered by a masked scan per artifact, and **the cut cannot save it** — the cut runs
after the verdicts, so it serves fewer artifacts and never evaluates fewer, and a viewport over such
a layer pays every one of its artifacts whatever `artifact_budget` the client sent. The budget looks
like a cost control here and is not one.

**The measurement then fixed two things about the shape of the answer**, and neither of them turned
out to be a bound. **The per-request bound this section used to require — refusing on the layer's
declared artifact count before any evaluation — is withdrawn**
([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md),
2026-08-21): what costs is row-space locality rather than the count, so a threshold on the count
refuses the cheap layer and admits the dear one, and every build reports blocks per artifact
instead. The text below stands as the record of what the two measurements said.

*It has to bite well below a million artifacts — unless the counts come from the column.* At 10⁶
predicate artifacts over 10⁷ points the per-artifact loop costs **455 ms per request** over the
whole map and **15.8 s per generation move**. Comfortable is a few thousand: 23 ms at 1 024, 108 ms
at 10 000. Memory behaves as a reader expects, 108 MB for that million. The crossing arm at the same
size is eleven hours per request and its gap *widens* with the layer — 3 700× at 64 artifacts to
88 000× at a million — so nothing about scale rehabilitates it.

**A third route removes the artifact count from the request path, and it needs no new
storage.** A single-valued attribute predicate *partitions* the corpus — the column's distinct
values are its artifacts and every point carries one — so the column the predicate names already
says which artifact each point belongs to, and one pass answers every artifact at once. **It lives
in `attrs/`, not the render table**: `membership = { attribute = … }` names an indexed column, and
an indexed column is a `ValueColumn` — a dense typed code array plus a presence bitmap, addressed by
entity — which is what `category_membership` already walks. Entity space is where the counting pass
wants to be anyway, since a masked count is `|membership ∩ M_auth|`.

**It is flat in the artifact count and the per-artifact loop is not**, so the two cross: near
10 000 artifacts on a 10⁶-point corpus and near 30 000 on a 10⁷-point one. At a million artifacts
over 10⁷ points the histogram is **175 ms against 462 ms — 2.6×**, and over 10⁶ points 8.6 ms
against 68.5 ms. It is a latency choice that puts **nothing on the wire**: both compute the same
quantities from inside `M_auth`, and each wins on its own side of the crossover. What it does reach
is the timing channel — annotated on C4 and C15 rather than claimed absent (architecture r49).

**Two passes with different domains, and that is a disclosure rule rather than an optimisation.**
The count is over the whole membership (`annotations.md` §4.2) so its pass walks the mask; candidacy
is against the viewport, so that pass walks `viewport ∩ mask` — **and it is the expensive half**,
roughly 120 ms of the 175 at a broad viewport, being one inversion per viewport row. The counting
pass alone is ~30 ms.

⊘ Three reductions are modelled rather than measured, and **the first of them is now foreclosed**:
the counting pass is a function of `M_auth` and the layer and not of the request, so it could be held
per session on the mask fragment's cadence — which
[decision 0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
rules out, because it is sized by the artifact population and there are a great many tokens. The
other two stand:
the candidacy inversion is exactly what a **rendered** copy of the column would remove; and with
counts coming from the column, row forms are needed only for the artifacts actually **served**,
which the budget bounds, so the generation-move cost becomes a handful of lazy projections. ⊘ It is
for a single-valued **category** predicate: a multi-valued column does not partition, a **keyword**
column's ordinals are per layer and would need each layer's dictionary to merge, and a spatial
predicate has no column but makes `range_cardinality` cheap per artifact anyway.

**And the column route turned out to be the general one.** The scale campaign
([the memo](design/artifact-serving-at-scale.md), [the probe](../probes/2026-08-20-artifact-serving-scale/README.md))
found that what decides cost is **row-space locality** rather than the membership source: a
scattered layer has none of it — every artifact is too wide for any node of the index, at 734–1 524
row blocks per artifact against 1.0 for a clustered one — and no spatial structure helps it. So the column histogram above is one instance of a **row-major** layout
— a label per row where the layer partitions, a list per row where it overlaps — which is flat in
the artifact count, applies to enumerated and per-analyst layers as well as to predicates, and at
10⁹ points is the only layout that fits at all: 4 GB against 78.5. It also moves from entity space
to row space, which is where ~120 ms of the 175 went. Which layout a level gets is chosen
automatically and re-evaluated at each fold
([decision 0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)),
and containment stops being a per-request scan at all
([decision 0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)).

**The check:** the tagged-programme layer is built twice — once enumerated, once as an attribute
predicate here — and the two return **identical** masked counts for every principal and every
viewport; a point ingested inside a boundary is a member on the next request with nothing rebuilt.
The layer earns the attribute source by declaring **derived** geometry: a tagged set with nothing to
draw is a category, and the model sends that case to a column rather than to an artifact. **Data:**
`programmes/portfolio` and `regions/synthetic-geo` (§5.2).

### Stage 7 — Scale and the write cycle under load

**Pulled forward ahead of runtime artifacts** (owner, 2026-08-21). It used to be the second half of
the filters-search-and-scale stage; the scale investigation measured the serving path and made the
validation the next thing worth doing rather than the last.

**Capability:** the target scenario runs — one shared bundle at 10⁹ points carrying 10⁶ artifacts,
with 10⁷ as targeted probes, 1M+ unique access terms, and many concurrent principals each with
their own `M_auth` — while the write path stays correct at every interleaving of point and artifact
writes.

- **Fixtures**: the seeded generator's artifact arm reaching disk — a partition arm, attribute
  column and members-file emission, spatial boundary layers, and an **artifact census verb** giving
  expected masked counts per (grant, artifact) in closed form, which is what replaces an enumerated
  twin at 10⁹ where the twin's own member file would be ~10⁹ rows. `TERM_SPACE` parameterised so a
  campaign bundle carries ~10⁶ terms and 10⁶ artifacts together.
- **The campaign matrix**: three corpus tiers × three artifact counts × the layouts, with forced
  overrides on both sides of each crossover, principal breadths broad/median/narrow, and a
  **concurrency sweep** at 1/8/32/128 sessions **reporting the envelope** rather than a pinned
  pass/fail count. **Serving during a fold** at 10⁹ with live sessions is the named gap — the fold's
  artifact pass has only ever been measured on an idle box. Ingest-during-serving freshness, and a
  layout flip observed by live sessions, go with it.
- **The write-path interleaving battery**: publication against a batch's admission and close, two
  batches naming one unminted key, growth racing window close both ways, a fold racing a growth with
  crash-replay equivalence, suppression racing publish and mid-fold, window atomicity around the
  single fsync, and a threaded case asserting monotone freshness. **Zero tests construct these
  interleavings today**, which is why the battery lands before the serving build moves the write
  paths.
- **The configuration matrix, the census extension and the conformance fill**: the build refusals
  nothing reaches today, the census parametrised over entry point × membership kind × shape, and a
  `layers` field on the conformance battery's viewport query — **no recording carries an artifacts
  frame at all** at present, so that is the artifact channel's only cross-stage regression net.
- **Cache cadence, which the whole of it rests on**: the artifact store's version is global, so one
  write invalidates every cached row form in every view, and the lineage is rebuilt per request from
  something that depends on neither the mask nor the viewport.

**The check:** at 10⁹ points with 10⁷ artifacts a viewport serves inside its budget across the whole
grid of principals and zooms — both axes swept through their middles, because sampling their
extremes understated the worst request by 2.1× — the fold completes inside `plan_fold`'s memory
ceiling under a live serving load, and the row-major counts match the generator census **exactly**.
**Data:** the seeded generator at the scale tiers (§5.1, §5.3). **The plan:**
[the campaign memo](evidence/memos/2026-08-21-artifact-scale-plan.md).

### Stage 8 — Runtime artifacts

**Capability:** an analyst assembles a set mid-session, shares it, and edits it without breaking the
share.

- The create/edit control verb (contracts work): `(layer, membership, gate, content, key?)`,
  members named by `external_id` or `tessera_id`, resolved at admission.
- The edit verb, from the edit design pass this stage waits on (decision 0077 defers it): content
  in place with `G`; membership in place with a version bump; gate widening in place; **gate
  narrowing = suppress, re-grant, unsuppress**. Identity, edges and suppressions survive every edit
  (decision 0081).
- Optional keys; the replacement report (a replacement that strands live suppressions is
  reported, not refused); the dangling-dependent refusal, which binds replacement only.
- **The layout-forcing control verb**, deferred here from the scale campaign
  ([decision 0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)):
  it sets a layer's override, which takes effect at the next fold, rather than rewriting a live
  level — the operation the fold exists to batch.
- **The HTTP growth route**, which the design mentions and which does not exist; recorded here
  rather than built during the campaign.

**The check:** a set of ten shared with a colleague who cannot see three of its members shows
**seven**; a hundred edits later the day-one bookmark still resolves; a replacement that would
dangle an edge is refused with the offending dependents named, and one that strands a suppression
is reported. **Data:**
`selections/analyst-*` and `programmes/portfolio` (§5.2).

### Stage 9 — Filters, search and excluding

**Capability:** artifacts are searchable and filterable within their disclosure rules.

- Membership as a filter — unrestricted for layers declaring no criterion, criterion-inherited for
  those declaring one, and refused work-indistinguishably for a non-visible artifact.
- Search over artifact text via the token index on the artifact population, with C25 re-read against
  a population two orders smaller.
- **The replica-carried artifact tiers**, which Stage 7 declines in favour of the closed-form
  generator — a census answers where no expectation can be stored, and real geometry earns its
  fixtures here, where search and filtering are what need it.
- **The `excluding` complement, priced** (owner, 2026-08-20 — deferred here from the configuration
  work, where it is recorded as correct per the design and unresolved). A membership spelled by
  exclusion is complemented once, in the build, against the entity space the build assigned; three
  excluded ids over a 10⁸-point corpus therefore materialise a membership of ~10⁸ entity ids. It
  lands on membership, which is what the cut walks and what every masked count is taken against, so
  it belongs with the scale tiers rather than with the surface that spells it. The complement is
  build-time only and **no request-time complement is expressible** — that part is settled and is
  not what needs pricing.

**The check:** per-tile counts under a membership filter never resolve below the layer's existence
criterion; a filter naming an invisible artifact and one naming a nonexistent artifact do the same
work. **Data:** the real 2.4M corpus's layers (§5.2), and the scale tiers where the filter's cost is
what is in question (§5.3).

**The scale half of this stage's old check has moved to Stage 7**, where it is measured rather than
claimed. What was written here — that the serving half *fails as the path stands*, at 2 770 ms
single-threaded over 10⁷ artifacts — was true when it was written and is what the scale campaign
answered. Two corrections a reader of the old text needs: the mask half does **not** want
`architecture.md` §8.5's per-token servable-label set
([decision 0093](decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)),
and the artifact-major index is not the whole answer either, because a layer with no row-space
locality has nothing to index
([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md),
[0094](decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)).

## 4. What holds the line between stages

Stages 2 and 3 ship a bundle that carries layers before the write cycle exists. Two different
things are true of that state, and conflating them is how a fail-open ships:

- **Ingest is safe and stale.** A member ingested since the last fold has no bit in the row form, so
  every count **understates** — fail-closed, and the same posture as a buffered point being
  invisible until its flush.
- **A fold was unsound and is not any more.** Row space renumbers globally, so every resident
  membership form is meaningless at the flip — which is why the fold refused outright while the
  pass was missing, rather than serving stale (`compaction.md` §6.2 says so for this class of
  artefact). Stage 4's pass replaced the refusal: the durable form is rewritten into the new prefix
  from entity space, and the row forms are rebuilt inside the fold. What the pass does not yet do is
  tell `plan_fold` what it costs.

## 5. The test data

Four fixture families, because they answer four different questions. The word *artefact* below is a
stored file; *artifact* is the annotation object.

### 5.1 The seeded generator — the only fixture that scales

`tessera-corpus` gains an artifact arm, in Rust, under the same rules the rest of it obeys: every
property of artifact *a* is a keyed function of `(seed, layer, level, a)`, constant-time, no I/O, no
table, prefix-stable in *n*. Both directions must be closed-form — the members of an artifact, and
the artifacts holding an entity — or the census cannot answer "nothing is missing or extra".

Shape: a keyed contiguous Morton interval per artifact plus a keyed scatter, so compact *and*
pathological membership both occur by construction; the own-terms flag and criterion cycled across
artifacts so all four cells of the two-by-two are exercised at every size; a generating set as a keyed ~10²-member subset; deliberate overlaps
within a level, one single-member artifact, one artifact with zero visible members for a chosen
grant, one level with no artifacts at all.

**This is the only family that can carry Stage 4**, because the stage battery compares recorded
answers across eight stages at sizes where no expectation can be stored. It is also the only one
that reaches 10⁹ without a bundle on disk, following the decomposition probe's approach.

### 5.2 Real artifacts over the real 2.4M corpus

The corpus is 2,422,486 real arXiv papers with real BGE→PCA→UMAP coordinates, real categories and
real author surnames — so the *access terms* are real, which is what makes a containment test mean
something. Six layers, each earning its place by being the case some stage cannot test without it:

| Layer | Artifacts | Structure | Membership | Access | Content | The case it carries |
|---|---:|---|---|---|---|---|
| `clusters/hdbscan-2026-08` | ~10⁴ nodes | **a tree in its edges**, no levels | enumerated | no own terms; criterion | count, centroid, hull | the baseline. 20–25% noise means children are subsets of their parents but never exhaust them, which is exactly the shape per-node testing exists for, and the tree the budget cuts |
| `topics/ctfidf-2026-08` | ~3 per cluster, plus per-term variants | attached by edge | enumerated (the sample) | no own terms; no criterion — containment decides | label text from real titles | the containment result that surprises: broad and narrow viewers fail the *same* label |
| `centroids/kmeans-2026-08` | 4,000 | flat | enumerated | no own terms; criterion | **supplied** centre+radius fitted over full membership | model §8.6's trap — supplied geometry that looks derived; a viewer failing its containment sees no artifact (decision 0076), and the everyone-visible remedy is a last-ranked content |
| `regions/synthetic-geo` | ~2,000 over three scales | **levels *and* lineage** — they agree | spatial predicate | own terms `public`; no criterion | authored polygons and names | the tiered case decisions 0082 and 0087 name, where a level number and a tree depth mean the same thing — the only shape in which reading one as the other is safe. Also the perimeter cost and §5.1's "draw all boundaries or gate them" trap |
| `programmes/portfolio` | ~30 | flat | **attribute predicate**, and an enumerated twin built from the same rule | own terms: any principal; no criterion | authored name, **derived** extent | Stage 6's equality check — the same layer by rule and by list must return identical masked counts — and model §8.5's programme with a **zero** count and no hull |
| `selections/analyst-*` | ~50 | flat | enumerated, scattered | own terms: per-analyst; no criterion | none | model §8.3 — the set of ten that shows seven |

**A seventh layer was dropped rather than descoped.** arXiv subject classes were carrying the
attribute-predicate arm, and the configuration exercise found the model sends exactly that case to a
**category**: the value belongs to the corpus's own vocabulary, thousands of items carry it, and
nothing is drawn. A fixture built on it would have tested artifacts against a thing the design says
should not be an artifact. The tagged programme takes the arm instead, and earns it only because it
declares derived geometry — without something to draw it is a category with extra steps.

**One register claim is checked without a fixture of its own — ✔ done 2026-08-16.** C1 records that
a criterion bounds a grouping's existence and shape and never its count, because the density
underlay already serves exact masked counts at any depth. The check was therefore against machinery
that already exists, and needed no new layer:
`a_withheld_compact_cluster_has_its_count_recovered_from_the_underlay` publishes a cluster whose
membership is every point in one depth-1 cell, sets a bar the narrow principal cannot clear, and
finds the artifact **absent** while that principal's own tile count over the cell is **exactly** the
number withheld — 816 of them. The point is that the recovery succeeds and that the register says
so; the assertion that would fail is the one where the two disagree, which would mean C1 overstates
the exposure. Nothing here is a leak — every number summed is a masked count the principal already
holds — and the value of the test is that it fails the day anyone describes the criterion as
protecting counts.

**Four things this fixture must get right, each of which has already gone wrong once:**

*The clustering ships as a condensed tree, not as cut levels* — and this is new, from
[decision 0082](decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md). The campaign
produced three independent HDBSCAN runs at three `min_cluster_size` settings, which is the **stacked**
case and has no lineage at all; the fixture this stage needs is one run whose condensed tree is
exported with its parent/child edges intact. That is the shape the library already computes and the
one an earlier plan discarded by cutting it into levels. Both fixtures are worth having — the stacked
one is a real configuration and tests the other half of §6.2 — but they are **two layers, not one**,
and the treed one is the baseline.

*The clustering is HDBSCAN on a 250k sample with every row assigned by nearest centroid and the
noise fraction restored by a distance cut* — that is what the campaign ran, and it bounds what the
fixture licenses: the cluster count and noise fraction are HDBSCAN's, the fine boundary detail is
not. ⊘ **Nearest-centroid assignment does not extend the condensed tree**, which is a consequence of
the change above rather than a known problem: a row assigned to a leaf by distance has no place in
that leaf's ancestry unless the assignment is propagated up the edges, and whether that reproduces
HDBSCAN's own membership is unverified. Worth one attempt at full 2.4M HDBSCAN on the GPU over the
2-D projection before accepting the sample, which would dissolve it; if that does not run, the
propagation rule and this caveat are carried in the fixture's manifest rather than in someone's
memory.

*Geographic shapes are drawn at native density.* The campaign's 56× corridor figure was its own
subsampler holding member count constant with a stride, which forces one run per member for any
shape. Corrected, real coastlines cost **1.4×** and archipelagos **4.6×** a compact blob. A fixture
that decimates reproduces the bug rather than the shape.

*Labels are real text with a real generating set.* c-TF-IDF over the titles of a declared sample per
cluster gives English topic labels and a `G` that is an actual set of documents — which is the whole
point, since a synthesised `G` cannot fail containment for a real reason. Titles join from the
Kaggle snapshot, so this rides the full-schema rebuild rather than adding a second one.

### 5.3 The larger tiers — synthetic in exactly the way the corpus already is

The 250M and 10⁹ corpora are the real 2.4M repeated as ~103 and ~413 replicas, each an affine
transform plus jitter, with five replicas pinning deliberate edge cases. **Artifacts ride the same
machinery:** each replica carries the Tier A layers through *its own* transform, so a replica's
cluster is a real cluster's shape under an affine map — real size skew, real noise placement, real
Morton contiguity — and the degenerate replicas (extreme compression, corner-pinned, collapsed
line) produce degenerate artifact geometry for free rather than by invention.

Label text per replica is recombined from the real title vocabulary deterministically from the
replica index: it reads like an arXiv topic and is a sentence nobody wrote. Generating sets are
sampled per replica exactly as at 2.4M.

Arithmetic, *modelled*, and **restated on tree nodes rather than cut levels**
([decision 0082](decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md)): the count
that matters is a condensed tree's total node count per replica, not a chosen level's width. At
`min_cluster_size ≈ 6` the 2.4M tree is expected to carry ~10⁴ nodes, so 413 replicas give
≈ **4×10⁶** artifacts at 10⁹. Reaching the design's 10⁷ ceiling needs a smaller
`min_cluster_size` or a top-up from the seeded generator — **decided by measuring the tree, not by
predicting it**, and the measurement is now one number per run rather than one per level, which is
the small practical gain the ruling brings here.

**What does not transfer, stated so nobody quotes it later:** cluster *semantics* (a replica's
cluster means nothing), and the fact that the same paper appears 413 times, which makes any
statement about label uniqueness or vocabulary growth at scale a statement about the generator.

### 5.4 Two rules the fixture producer must follow

**A scale is a prefix filter on entity ID**, so membership and generating sets must be materialised
**per scale**, never filtered blindly. A prefix cuts at replica boundaries above 2.4M but cuts
*inside* replica 0 at 250k — where a cluster loses members and a generating set loses members it
declared. Filtering membership is correct; filtering `G` and keeping the old `|G|` makes every label
unsatisfiable, and filtering `G` and recomputing `|G|` is the shrink the design refuses. `G` is
**re-declared** per scale from the members that scale has.

**Membership goes to disk in entity space**, whatever the campaign says about row space. Row space
is per view and derived; a fixture that ships the row form has frozen a projection and will be
wrong after the first fold.

### 5.5 Who writes what

The seeded generator is **Rust**, in `tessera-corpus`, for the reason that crate already gives: it
defines the corpus rather than the system's behaviour, and two generators would put the fixture
under test. The arXiv layer producer is **Python**, in `probes/`, beside `build_corpus.py` and
`build_scaled_corpus.py` — offline data preparation, which is where Python belongs here. Neither
line moves: nothing Python reaches a request path, and the bundle artefacts are written by
`tessera build`.

## 6. What each stage owes a measurement

The campaign settled storage and dissolved one leak-register escalation. What it did not settle is
allocated here rather than left as a list:

| Owed | Stage | Why it could refute something |
|---|---|---|
| ✔ **Residency** — **measured 2026-08-16**: ~80–94 B per Roaring container, flat over 10⁴–10⁷ artifacts, so **6.16×** serialised on contiguous membership and **7.79×** on the synthetic arm. 794 MB is ~4.9–6.2 GB resident; the pessimistic arm at 10⁷ artifacts does not fit in 47 GB. [The probe](../probes/2026-08-16-membership-residency/README.md) | 2 ✔, re-measured at 7 | the view budget still has no line for the multipliers, which stand on top of this |
| ✔ **The fold's artifact pass** — **measured 2026-08-16**: projecting per artifact through the new permutation beats riding pass 1 on memory (**+3.5 GB against +9.2 GB**, page cache against anonymous), and threads where riding cannot (**32.8 s on eight threads against 101.3 s** at 10⁹ rows / 10⁷ artifacts, linear in rows). Half the comparison dissolved on re-posing: membership is entity-canonical, so no `old_row → new_row` table exists to build. [The probe](../probes/2026-08-16-fold-artifact-pass/README.md) | 4 | `plan_fold`'s ~9–10 GB anonymous peak **stays where it is** — the pass adds the output row forms and page cache |
| **The three arms** — flush-union, merge-rebase, fold-rebuild | 4 | the merge arm's bound is proportional to the merged span, not to the artifact population |
| **Σ\|G\| and containment per request** | 3 | ~4 B/member *assumed*; refuted if a real deployment's Σ\|G\| approaches membership's order |
| **Reach**, if it is materialised at all | 5 | the deleted assignment-column framing made it look free; it is a second membership structure |
| **Predicate evaluation per request** — ~10³ ranges for a 40,000-member corridor, so ~0.3–1.2 ms per artifact per request *(modelled)* | 6 | a nationwide boundary level was seconds per query, which is what the withdrawn bound was for; the answer is the layout the level is given, not a refusal ([decision 0092](decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)) |
| ✔ **The viewport's artifact pass** — **measured 2026-08-21**, and never measured before: the shipped request path is `O(artifacts)` **four** times and three of the four have no request in them. At 10⁷ artifacts over 10⁹ points, single-threaded, the shipped request path is seconds per request; a hierarchical row-range index for the viewport half and a **build-time partition over terms** for the mask half bring it to **553 ms at 10⁷ artifacts over 10⁸ points and 131 ms at 10⁶ over 10⁹** *(re-measured 2026-08-22, medians of three runs; the 31.7 ms first reported was a route that omitted the masked candidacy the design requires, and candidacy is 549 of the corrected 553)*, with every verdict identical. Two things it found beside the figure: the shipped path gets *slower* as a principal gets narrower (1 349 ms at a full mask against 4 443 at 3.1%, at 10⁶ artifacts) where the design's route gets faster, and §8.5's per-token servable-label set is **not** what the mask half wants — containment is a boolean expression over terms, so it is build-time and needs no per-token state, which matters because there are a great many tokens. [The probe](../probes/2026-08-20-artifact-serving-scale/README.md), [the options](design/artifact-serving-at-scale.md) | 7 | the scale check — *"at 10⁹ with ~10⁷ artifacts, a viewport serves inside its budget"* — did not hold, and now does |
| ✔ **Locality decides the layout, and what decides it is residency rather than speed** — **measured 2026-08-21, corrected 2026-08-22**: a layer whose membership is *scattered* (an attribute predicate, a per-analyst selection, a term-as-artifact) has no row-space locality at all — every one of its artifacts lands in the walk's `everywhere` set at every size measured, at 734–1 524 row blocks per artifact against 1.0 for a clustered one. **There is no serving-speed wall up to the 10⁵ artifacts measured** (45–56 ms at whole-map zoom); the ~2×10⁵ wall first reported was the structures that preceded the hoisted route, not the shape. What still forces a **row-major** layout — a label per row where the layer partitions, a list per row where it overlaps — is residency: at 10⁹ points it is the only layout that fits, **4 GB against 78.5**, and it is flat in the artifact count where the artifact-major form is not | 7 | the artifact-count target is a statement about layers with **locality**; without it the layout has to invert (options §5) |
| ✔ **A nested hierarchy costs neither what its member count nor its cut suggests** — **measured 2026-08-21**: every level covers the corpus, so the layer holds `depth × rows` of membership — but a node is a contiguous range and therefore one run whatever its size, so the whole layer is **1.2 row blocks per artifact**. And the cut over it is **0.3–12.9 ms** at every principal and zoom measured *(re-measured 2026-08-22)*, against 176 ms on a fixture whose tree was unrelated to its geometry. The **masked count of a coarse node** behaves as restated — mask-dependent and falling with the viewport, 4.4 ms at a full mask and 49.5 ms below one at 10⁶ nodes over 10⁹ points, scaling with the corpus rather than the node count — but it is no longer what bounds such a layer: masked **candidacy** is, at 242 ms of the worst cell's 307 | 7 | ⊘ per-signature counts for the coarse nodes would remove the count's cliff for ~1.3 MB, and their storage scales with the **signature** count rather than the artifact count — the fixture's 32 against the census's 54,794 over the demo corpus |
| **A real clustering at 10⁹** | never here | Tier A and Tier B agree on direction and not on constant: real membership is 14–170× cheaper per member than the synthetic arm |

Two arms of the existing campaign can be finished now that they could not be then: `data/corpus.parquet`
and `data/geometry.parquet` are both present on this machine, so `m7_core_rows.py` runs, and the
permutation-compression and `Permutation::project` questions the campaign left open are cheap to
close before Stage 1 commits to the tiebreak.

## 7. What could still refute the shape

**Residency, not storage — answered, and it does not refute the shape.** Every sizing number in the
design is serialised bytes; the question was whether 10⁷ resident bitmaps cost materially more.
They cost **6.16× on contiguous membership and 7.79× on the pessimistic arm**, flat across three
decades, because the cost is ~80–94 B per Roaring *container* rather than per artifact or per member
([the probe](../probes/2026-08-16-membership-residency/README.md)). The design's own point is 3.6 GB
resident against 582 MB serialised — large, bounded, and servable. **The assignment column stays
deleted.**

What the number does change is the packaging question below it. Serialised is 15.2 B/container
against 94.0 resident, so a form usable *in place* — mapped rather than deserialised — costs roughly
its disk size and moves the cost from anonymous memory to reclaimable page cache. That is a ~6×
argument for the frozen-format option, and it is now measured rather than aesthetic.

**What is not answered** is the multipliers: by view, by level, and by two during a replace. The
engine also holds two resident copies today — the entity-space store and the row-space projection —
so the design's point is 3.6 GB *per copy* before any multiplier. Re-measured at Stage 7.

**The fold — answered, and it does not refute the shape.** The artifact pass fits: projecting each
membership through the new permutation adds the output row forms (~3.5 GB at 10⁷ artifacts) and page
cache, leaving `plan_fold`'s anonymous peak where it is, and it threads to **32.8 s** at 10⁹ rows
([the probe](../probes/2026-08-16-fold-artifact-pass/README.md)). Riding pass 1 — the alternative —
costs +9.2 GB anonymous and cannot be threaded at all. So neither the first-toucher stall nor the
blank level after a nightly fold is forced. What is still open is the pass **under a concurrent
serving load**, which the probe measured on an idle box.

**The drill-down conflict.** The resolved visibility set restores the structural closure a
per-identifier count would break, and the live-count rule says a cache may bake in counts but
never verdicts. Those two are in tension on exactly one route, and the design says so. It needs a
small design pass during Stage 2, not a decision taken by whoever implements the endpoint.

## 8. Relation to the epics

[#13] and [#41] remain open and describe the same capability from the outside — cluster structure
from the viewer's own visible set, and labels gated on containment. **They are not the status
record for this work**; this file is, by owner direction. If the artifact work is ever moved back
onto issues, this file is deleted rather than left to disagree with them.

[#13]: https://github.com/jennis0/tessera-index/issues/13
[#41]: https://github.com/jennis0/tessera-index/issues/41
