# Disk-use campaign — what the owner ruled, 2026-09-10

**Status:** Ruling record. The owner's decisions on the questions the eight
[`2026-09-10-disk-*.md`](.) fact-finds raised, taken in one session on 2026-09-10 against main at
0c0b60b3. Each item names the evidence it rests on. **This is a record of what was decided, not a
design** — the work each ruling implies is not specified here and is not built.

## The rulings

**1. The build's disk pre-flight warns; it does not refuse.** Fix the model's wrong terms, and make
the outcome advisory rather than a gate. The reason is that the model can be wrong in either
direction, so it should not hold a door: seven of its terms can be exceeded, five are exceeded by
corpora in the tree, and whole bundle files carry no term at all (176.81 B/item unmodelled on
`medcpt-10m-abs`, 23% of that bundle — [forecast](2026-09-10-disk-forecast.md)). Companion fix: on
`ENOSPC` only `.build-tmp/` is swept today and the partial bundle under `<out>/v00000/` stands, so a
retry starts with less space than the first attempt. Sweep it.

**2. The WAL pin — correct the document, then build the cheaper release.** `ingest.md` §2.4 describes
a per-record release that was never built; the code pins every membership growth until the fold and
its own doc argues why the naive release loses a join silently. Correct §2.4 to match the code. Then
build a membership-only repack that releases the pin without a full fold: `ArtifactStore::repack_all`
is already a separable, IO-free function, and such a repack needs roughly 2× the membership store
(15–38% of live) against the fold's 150% ([membership](2026-09-10-disk-membership.md),
[ingest](2026-09-10-disk-at-ingest.md)). Ruled necessary rather than conditional, because without it
a deployment can reach a state where the only operation that reclaims cannot run.

**3. The compaction trap — warn, fold earlier, and add a retention sweep.** Serving holds 1.3–2.6×
live; the fold demands a further 1.5× live free, so a box at 2× live can serve a corpus and cannot
fold it. The refusal becomes a warning (a failed fold is recoverable: the old prefix is untouched
and the partial new prefix is swept at startup), the trigger fires earlier than dead/live ≥ 1.0, and
a bounded retention sweep reclaims dead files inside the live prefix without a fold. The sweep's one
design question is its retention depth, which must cover any step-down that can still be served.

**4. The fold's margin names its spools.** `FOLD_DISC_PERCENT = 150` is marked "Assumed" and does not
name the pass-1 column spools, which the code prices as corpus-sized (~a quarter of live). Compute
the spool term from the declared schema so the estimate moves with the schema; let the total land
where it lands. A measured fold peak was offered and declined.

**5. The allocation tiebreak is the source-id ordinal.** See
[decision 0112](../../decisions/0112-the-allocation-tiebreak-is-the-source-id-ordinal.md), written
for the first time — `views.md` cited it twice and no such file existed.

**6. `distinct_of_ordinal` is mapped, and keeps its budget term.** Make it file-backed to recover
13,335 MiB of anonymous memory at rung 6, and leave its `4 * n` in `loop_fixed` so `auto_batch` is
unchanged and entity ids are byte-identical. Removing the term later, for larger batches, is a
separate decision.

**7. Derived files must not collide across views.** `derived_name` puts no view in a filename and
`artifact_pass::run` restarts its index per view, so the second view's files overwrite the first's.
Measured: `treeoflife-1m`'s three `row-column` files are all 1,520,534 B, the `geo` view's 760,259
rows at width 2 plus a 16 B header, where `bioclip`'s 1,000,000 rows would need 2,000,016. The fix
makes the index unique across views — a running counter or a positional ordinal — and does **not**
put a caller-shaped name in a path, which `derived_name`'s doc rules out for its own reasons.

**8. `contracts.md` §2.4's "the postings are emitted and not read at serving" is false** and is
corrected. They are read on the request path by `eq`/`in` resolution and by the `derived` visibility
gate — the consumer `postings_are_owed`'s own doc names.

## Recorded, not decided

- **`SortRec` at 12 bytes or 16.** The record grew to 16 to hold the anchor Morton code, measured at
  4.08× on artifact membership's disk form. Its doc names the alternative it did not take — keep 12
  bytes and read the code from the mapped array inside the comparator, trading 4 B/item of anonymous
  memory for an indirection per comparison. **14 GB at rung 6, unmeasured.**
- **`auto_batch` reads host memory.** With no `--memory-budget`, `detect_memory_budget()` reads
  `MemAvailable` and the cgroup limit, so a budget-constrained corpus's permanent entity ids depend
  on the machine and the moment. Mitigated by provenance recording the stride and `--batch-items`
  replaying it. Bites rungs 5 and 6 alone. Undecided, and I9 makes it permanent at release.

## Not raised as rulings

The fact-finds carry findings that are work rather than decisions — the indexed `keyword` column's
65.7 GB at rung 6 on the same argument as the in-flight blob-resident change, the `text` column's
never-marked presence bitmap, `residency.rs` charging the source ids `8 * n` where the file is the
pre-dedup per-view total, and the fragment cache's fold-only sweep. They are in the sibling memos
and are not decided here.
