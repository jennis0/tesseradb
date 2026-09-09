# Where the layers stage and the artifact pass spend their time

**Date** 2026-09-09. **Branch** `perf/layers-cost`, against main at 3475448e. **Box** WSL2, AMD
Ryzen 9 5900X, 12 cores, 47 GB, local NVMe-backed VHDX. **Corpus** `data/ladder/gbif-240p` —
102,592,404 placed GBIF occurrences, one `geo` view, one tiered taxonomy of three levels
(family/genus/species), 494,120 artifacts, 294,903,789 membership entries. `data/ladder/gbif-64p`
(25,846,007 rows, 74,626,027 entries, 251,907 artifacts) is the same shape a quarter of the size and
is what the per-change figures below were iterated on.

Two stages ran at ~1.1 of 12 cores and were 43% of a 335-second build: `layers` and the artifact
layout pass, which is billed to the `manifests` stage because it runs between that stage's start and
its end. This measures what is inside them, removes four kinds of work from them, and says what is
left and what would parallelise.

    export CARGO_TARGET_DIR=<a target dir of this worktree>
    git stash && cargo build --release --bin tessera && cp target/release/tessera /tmp/before
    git stash pop && cargo build --release --bin tessera && cp target/release/tessera /tmp/after
    BEFORE=/tmp/before AFTER=/tmp/after CORPUS=data/ladder/gbif-240p WORK=/tmp/lc REPEATS=3 \
        bash probes/2026-09-09-layers-cost/run.sh

Every figure is **measured** unless it says otherwise.

## The result

Three before/after pairs on `gbif-240p`, alternating, the two binaries a `git stash` apart. Each
row is one build.

| pair | | `layers` | artifact pass | `manifests` | stages | whole build | peak |
|---|---|---|---|---|---|---|---|
| 1 | before | 90.2 s | 51.2 s | 52.1 s | 319.2 s | 5:33.9 | 11.18 GiB |
| 1 | after | **71.1 s** | **33.1 s** | 34.0 s | 294.1 s | **5:08.3** | 11.08 GiB |
| 2 | before | 115.0 s | 52.5 s | 53.4 s | 357.1 s | 6:14.4 | 10.25 GiB |
| 2 | after | **73.8 s** | **32.0 s** | 32.8 s | 314.1 s | **5:29.4** | 10.37 GiB |
| 3 | before | 152.8 s | 61.9 s | 63.9 s | 462.8 s | 8:02.3 | 7.15 GiB |
| 3 | after | **110.1 s** | **38.0 s** | 39.5 s | 383.6 s | **6:43.3** | 9.71 GiB |

⊘ **Only pair 1 ran on a quiet box, and the absolutes are not comparable across pairs.** This
session's own scripting ran through pair 2, and the workspace test build ran through pair 3; free
memory fell from 7 GB to none across the three, the build derives its memory budget from what is
available, and pair 3's `before` sized itself to a 7.15 GiB peak and took 8 minutes for it. What
survives that is the within-pair ratio, each pair being two builds a minute apart:

| | pair 1 | pair 2 | pair 3 |
|---|---|---|---|
| `layers` | ×0.79 | ×0.64 | ×0.72 |
| artifact pass | ×0.65 | ×0.61 | ×0.61 |
| the two together | 141.4 → 104.2 s | 167.5 → 105.8 s | 214.7 → 148.1 s |

On the quiet pair the two stages fall from **141.4 s to 104.2 s** and the whole build from 5:33.9 to
5:08.3.

One pair on `gbif-64p`, with `pairs.parquet` written, for a second corpus and a second shape of
comparison:

| | `layers` | artifact pass | `manifests` | whole build |
|---|---|---|---|---|
| before | 22.5 s | 18.4 s | 18.8 s | 1:28.9 |
| after | 17.9 s | 6.1 s | 6.4 s | 1:24.1 |

**The output is the same bundle.** Byte-identical across both corpora: 32 files at `gbif-64p` with
`pairs.parquet` written (`PAIRS=1`), 31 at `gbif-240p` without it, and the only two that differ are
`MANIFEST.json` — in `created_at` alone, checked field by field — and `CURRENT`. This is the
comparison `docs/ingest-campaign.md` §4c uses.

Peak `VmHWM` on the quiet pair moved from 11.18 GiB to 11.08 GiB: nothing here trades memory for
time. The peaks in pairs 2 and 3 say what the box had free rather than what the build wanted.

## Where the 141 seconds went

Measured on `gbif-240p` in a separate run of the unchanged binary with `eprintln` timers around each
block, removed before the change was committed. The timers cost under a second of the 141 except
where a note below says otherwise, and this run's own stage totals — `layers` 94.2 s, `manifests`
55.6 s — sit a few per cent above the clean pairs in the table above, which is the box's spread and
not the timers.

| | s | what it is |
|---|---|---|
| **`layers`** | **94.2** | |
| `layers::read` | 64.1 | reading `members-taxonomy.parquet` and spilling its runs |
|   — the Parquet decode | 10.3 | ZSTD, 157 row groups, one thread |
|   — the per-row loop | 53.0 | 102.6×10⁶ rows, 2.95×10⁸ entries; includes 2.6 s writing the runs |
|   — `apply_lineage` | 0.7 | |
| `layers::publish` | 29.6 | |
|   — `merge_member_runs` | 13.3 | the k-way merge, the source→entity gather, the per-artifact sort |
|   — `verify_hierarchies` | 4.6 | every child's members galloped against its parent's |
|   — the publication loop | 7.1 | one Roaring bitmap per artifact, then the registry's records |
|   — `write_membership_extents` | 4.1 | pack, fsync, remap |
|   — `resolve_artifact` (rayon) | 0.1 | |
| **artifact pass** | **54.5** | |
| `observe_shape`, three levels | 23.7 | of which **20.2 s is `project_base_with`**, 494,120 calls |
| the row-major columns | 30.2 | of which **22.1 s is `project_base_with`**, 494,120 calls |
| the tile indexes | 0.0 | all three levels are recorded row-major, and a row-major level has none |
| filing and containment | 0.7 | |

**Two thirds of the artifact pass is one primitive called twice per artifact.** Both walks project
the same membership into row space, once to observe the level's shape and once to compose the form
the shape chose; the tile-index walk would be a third on a level that stayed artifact-major.

**The `manifests` stage is the artifact pass and almost nothing else.** 52.1 s against the pass's
own 51.2 s, so the SHA-256 of everything the build wrote — 6.89 GB across the manifest's file list —
is **0.9 s**. It is already parallel (`rayon::into_par_iter` over the files,
`crates/tessera-build/src/lib.rs`), and it is not a cost worth looking at.

### Inside `project_base_with`

Two timers inside `Permutation::project_with`, on `gbif-64p`, before any change: the entity lookup
and bucket fill against the emit that turns the stamped bits into Roaring containers.

| | s | per call |
|---|---|---|
| lookup and bucket fill | 1.9 | 149×10⁶ entity lookups, 12.7 ns each |
| the emit | 5.5 | 4,055,754 containers, **1.36 µs each** |

A container of this corpus holds ~40 rows, and the emit read all 1,024 of its words three times over
— a popcount, a pass to write the members out, and a wipe — because that is the shape a
session-scale mask wants. The mark array below is what removes it for a container that does not.

### Inside the per-row loop

Per-row timers on `gbif-64p`, which cost about 3.2 s of the 14.9 s they measured, so each figure is
an upper bound by roughly a third of a second.

| | s | what it is |
|---|---|---|
| `resolve_member` | 4.6 | one `FxHashMap<Box<str>, usize>` probe per entry, keys ~30 bytes |
| `attach_member` | 4.4 | one `FxHashMap<u32, Vec<u64>>` probe and one push per entry |
| `record_lineage` | 3.9 | two `BTreeMap<usize, Vec<usize>>` probes per row |
| everything else | 1.9 | the list offsets, the null tests, the reused entry buffer |

### Inside `merge_member_runs`

On `gbif-64p`, four timers over the 3.0-second merge. **No term dominates**, which is why nothing
below touches it.

| | s |
|---|---|
| `next_artifact` — the k-way merge and the run reads | 0.9 |
| the source→entity gather | 0.7 |
| the per-artifact sort | 0.7 |
| the table write | 0.4 |

## What was changed

Four changes, each measured on `gbif-64p` against the binary immediately before it, and each leaving
the bundle byte-identical. The first two are in `Permutation::project_with`
(`crates/tessera-store/src/permutation.rs`), the second two in the member read
(`crates/tessera-build/src/layers.rs`).

### 1. The stamp is zeroed when it is sized and not on every call

`project_with` cleared and re-`resize`d its 512 KB bit array on entry. The emit already fills each
container it writes with zeros as it writes it, so the array comes back all-zero and the clear was
512 KB of stores per projection — 506 GB over `gbif-240p`'s 988,240 of them.

**Artifact pass 12.76 → 9.75 s** at `gbif-64p`.

The invariant this now depends on is asserted: `debug_assert` at the top of `project_with` requires
the stamp and its marks to be zero on entry, and a new test drives one `ProjectScratch` through five
masks of differing density and requires each answer to equal a fresh scratch's.

### 2. A sparse bucket's containers are read through a mark array

A second bit array, one bit per word of the stamp, records which words hold a row. A bucket holding
fewer rows than the stamp has words — under one row per word — emits each container by walking its
16 mark words and touching only the stamp words they name, and stages it through
`Sink::push_members` rather than `Sink::push_block`. A bucket denser than that keeps the
whole-container emit unchanged, which is the one a session's mask takes: a quarter of 10⁹ rows is a
million rows per bucket, sixteen times the threshold.

**Artifact pass 9.75 → 6.18 s** at `gbif-64p`; the emit itself **5.50 → 2.42 s** over the same
4,055,754 containers.

The two emits stage the same containers — same key, same cardinality, same ascending members, and
`push_members` writes the array payload `push_block` writes below croaring's array threshold — so
the bitmap is identical and so is the bundle. `tessera-roaring` already asserts that equivalence
across the threshold in both directions, in
`the_list_form_and_the_words_form_stage_the_same_container`, and a new test here projects a mask
that is dense in one bucket and sparse in the next, so both emits run inside one call over one
stamp.

This is the finding `Sink::push_members` already records for the other producer of sparse
containers: transposing rung 3's `mesh/descriptors` row form measured 16.7 s through `push_block`
against 5.0 s through `push_members` for the same 1.66×10⁹ members. The projection was the caller
that had not been moved across.

### 3. The spill's open window is indexed by the artifact

`MemberSpill::open` was an `FxHashMap<u32, Vec<u64>>` probed once per member entry. The key *is* the
artifact's position in the plan, so it is now a `Vec<Vec<u64>>` indexed by it; the spill walks the
slots in ascending order, which is the order it used to recover by sorting the key set.

### 4. A child's parent is one slot in a dense array

`record_lineage` probed a `BTreeMap<usize, Vec<usize>>` twice per member row — 1.94×10⁸ edges over
this corpus, from its manifest's per-level row counts, each a walk down about six nodes — to record
an edge that can only ever hold one parent, a second one being a refusal. It is a
`Vec<Option<usize>>` indexed by the child's plan position. `apply_lineage` still sorts by address
before it applies anything, so which of several conflicts is reported does not move.

**Changes 3 and 4 together: the per-row loop 12.35 → 6.61 s**, and `layers::read` 15.08 → 9.11 s, at
`gbif-64p`.

## What was tried and not taken

⊘ **A one-entry key cache per level.** `resolve_member` is the largest remaining term in the read,
and a cache of the last key seen at each level would remove the probe wherever consecutive rows
share a taxon. Measured over row group 3 of `members-taxonomy.parquet` (646,546 rows): **4.7%, 3.3%
and 1.4%** of entries at levels 0, 1 and 2 repeat the previous row's key at that level. A cache that
misses 96% of the time costs a comparison and saves nothing.

⊘ **Holding each level's projections so the artifact pass projects once instead of twice.** It would
remove about 9.5 s of `gbif-240p`'s artifact pass (modelled: half the measured projection time).
What it holds is a level of Roaring bitmaps over row space — ~350 MB for this corpus's deepest level
(modelled from 85×10⁶ members at 2 B each plus 3.9×10⁶ container headers), and ~3 GB for rung 5's,
which has 1.63×10⁹ members over seven levels. That is the term `MemberSpill` exists to keep out of
the build, and it grows with the corpus where everything else here is a constant. Left for the owner
to rule on rather than taken.

⊘ **Reading a text key without building a trait object.** `KeyColumn::read_at` reaches `is_null`
through `&dyn Array`, which is a virtual call per member entry; testing the null on the concrete
array instead measured 6.61 → 6.77 s at `gbif-64p`, inside the run-to-run spread and on the wrong
side of it. Not taken.

⊘ **Resolving source ids to entities at the read rather than at the merge.** The merge's gather is
2.95×10⁸ random reads into a 410 MB array; the read sees each row's source once, in ascending order,
and could resolve it there in one sequential sweep serving all three of the row's entries. It was
not taken because the run files would then hold entities, whose deltas are five bytes where a
source's are one — entity ids are issued in signature-sorted order, so an artifact's entities are
scattered across the space where its sources are dense in it — and the transient run files grow from
~350 MB to ~1.2 GB (modelled from the entry count and varint widths). The merge is also not
dominated by the gather (0.7 s of 3.0 at `gbif-64p`), so the win would be partial.

## What is left, and what would parallelise

**A recommendation. Nothing in this section is implemented.** "Serial now" is measured on
`gbif-240p` with the four changes above in, by the same timers the breakdown used; "modelled" is
that figure under an assumed 12-core scaling of the part named, and nothing in that column was run.

Ordered by what it would return against what it would cost.

| | serial now | modelled | shared state and hazards |
|---|---|---|---|
| The Parquet decode, read ahead of the row loop | 9.1 s | ~0 s — hidden behind the 27 s row loop | A bounded queue of decoded batches, two or three deep at ~70 MB each. The row loop stays serial and sees batches in file order, so nothing about the output moves |
| `observe_shape` over artifacts | 12.4 s | ~2 s | Artifacts, blocks and the `everywhere` count are commutative sums. `partitions` is the one that is not written that way: it is a running union with an early exit, and a parallel form needs a union per chunk and then the check that the chunks' cardinalities sum to the merged union's. The projection scratch is 520 KB per thread |
| `verify_hierarchies` over parents | 4.4 s | ~1 s | Each parent's pass reads the member table and writes only its own violations. The violation list must be re-ordered deterministically before it is reported |
| The publication loop's bitmaps | 6.3 s | ~2 s | `bitmap_of_entities` per artifact is independent; `prepare_publish` must still take them in key order, so this is build-in-parallel, publish-in-order |
| `merge_member_runs`'s per-artifact work | 11.8 s | ~6 s | The k-way merge is sequential by construction. The gather, the sort and the write are per artifact; the writer must reassemble in artifact order behind a bounded reorder buffer, the table being addressed positionally |
| The row-major column | 17.5 s | — | **Not recommended.** Sharding by artifact means unsynchronised writes into one 410 MB label array, which is safe only if the level partitions — and detecting that it does not is what this pass is for. Sharding by row range means every thread projects every artifact, which is twelve times the projection work |
| The member read's key resolution | ~15 s | ~4 s | **The largest single item left, and the hardest.** The roster is read-only only until a key misses, and a miss mints into the plan; the mint order must stay file order for the unclustered count and for which refusal is reported. Two-phase — resolve against a frozen roster, collect misses, mint them serially — plus one spill accumulator per thread, which changes the run count and the merge's fan-in |

Taking the first four — the ones with no ordering hazard beyond a deterministic re-order — is
modelled at `layers` 71 → 55 s and the artifact pass 33 → 22 s. The last two are most of the
remaining distance and both change the shape of a pass rather than its scheduling.

## What is not measured here

- **Anything but this corpus's shape.** One tiered layer of three levels, one view, enumerated
  membership, every level recorded row-major. A shape layer's resolution, a `dag` layer's closure
  and an attribute predicate's level are untouched by these changes and unmeasured by them.
- **The serving path.** `Permutation::project` is the session's projection as well as the build's,
  and the sparse emit is chosen per bucket by a threshold sixteen times below where a session's mask
  sits — so a session takes the whole-container emit unchanged. That reasoning is not backed by a
  measurement of a session-scale projection before and after.
- **Scaling.** The two corpora here differ by 4×, and nothing was run at rung 5's or rung 6's size.
  The artifact pass cost 50.7 µs per artifact at `gbif-64p` and 110 µs at `gbif-240p` before the
  change, so the per-artifact figures are not constants and none of this extrapolates by artifact
  count alone.
- **The run-to-run spread of the stages this did not touch.** `dictionary`, `geometry_read` and
  `assignment` moved by 30% and more between runs of the same binary on this box, which is why the
  headline table gives a range rather than a figure and why the whole-build number is the least
  reliable one in it.

## The three scheduling parallelisations, taken (2026-09-09, later the same day)

The first three rows of the table above are implemented; nothing else is. **Measured** on
`gbif-240p`, alternating main/branch/main/branch so that drift in machine state hits both arms, on
a box shared with other sessions — the load at each build's start is given because it varies and
the build sizes its memory budget off free memory.

| round | | `layers` | the artifact pass | all stages |
|---|---|---|---|---|
| 1 | main, load 5.83 | 110.58 s | 57.80 s | 405.97 s |
| 1 | branch, load 2.21 | **62.58 s** | 40.46 s | **313.96 s** |
| 2 | main, load 3.66 | 116.94 s | 73.87 s | 485.35 s |
| 2 | branch, load 4.48 | **58.67 s** | 34.41 s | **350.39 s** |

`layers` is halved — ×0.57 and ×0.50 — and round 2's branch ran at a *higher* load than its own
control and still won, so the win is not a scheduling artefact. Over three pairs taken today main
holds 110–117 s and the branch 59–63 s. All stages together fall about a quarter.

⊘ **The artifact pass is not attributable and no claim is made for it.** None of the three changes
touch it, and across six builds it ranged 34.4 to 84.6 s with no pattern — it is the stage this box
measures least reproducibly. An 84.6 s reading in an earlier pair looked like a 61% regression and
was the top of that spread.

⊘ **The gain is larger under contention than the model predicted** (×0.77 modelled, ×0.53 measured),
because the control degrades under load where the parallel form has slack to absorb it. On an idle
box the gap would be smaller, and no idle-box pair was taken.

**Byte-identical**, checked on the full 102,592,404-row corpus: only `CURRENT` and `MANIFEST.json`
differ, and the manifest only in `created_at`, compared field by field.
