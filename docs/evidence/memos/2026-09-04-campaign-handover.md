# Ingest campaign — handover of the open work, 2026-09-04

**Status:** handover. Written for whoever picks these up next; each item states the problem, where
the evidence is, and what done looks like. Nothing here prescribes a solution. Main is at
`c09ff11d`. The campaign's record is [`../../ingest-campaign.md`](../../ingest-campaign.md); the
measurement tooling and its schema are [`../../../test_corpora/common/README.md`](../../../test_corpora/common/README.md).

## Where the campaign stands

Rungs 3 (MedCPT, 36M), 4 (PaperSeek + OpenAlex, 102M, abstracts indexed, 70.8 GB bundle) and 5
(TreeOfLife, 233M, two views) are built, verified, served under a memory cap at half the bundle,
measured on the principal ladder, and in the rendered campaign table. The build no longer holds
prose in an arena (`docs/design/build-column-extents.md`; rung 4 builds in 1 h 09 m, byte-identical
to the arena build). Online ingest on MedCPT went from 11.8k to 62k rows/s
(`probes/2026-09-04-ingest-executor/`). Every track's branch is merged; no worktree of the
campaign's remains.

## 1. Streaming artifact publication

**Problem.** `test_corpora/common/ingest_cycle.py` publishes a layer's artifacts on the wire
after their points (`PUT /control/layers/{name}/artifacts`, ruled 2026-09-04) by inverting the
rung's member table in memory, one whole layer at a time. Two rungs' largest layers are therefore
never published in the ingest cycle, and their 0091 census rows differ by design: MedCPT's
`mesh/descriptors` (1,658,437,807 member rows, declined at `--max-member-rows`) and TreeOfLife's
`clusters/kmeans` (233,118,470). TreeOfLife's `taxonomy/tree` is never published for a second
reason: it is an open value set minted from list-keyed member rows with no artifact roster, and
the publication step reads rosters only.

**Evidence.** `test_corpora/medcpt/measurements.json` and `test_corpora/treeoflife/measurements.json`
(`publish.declined`); the driver's `publish` step and its module doc; the route's body form in
`crates/tessera-server/src/control.rs` (`PublishBody`, `IncomingArtifactBody`, 64 MiB cap, the
batch is the commit unit).

**Done looks like.** Every declared layer of every rung publishes in the cycle without the driver
holding a layer's membership whole — the member table read in artifact order once, or the route
taking growth in pieces — and the census rows for those layers read exact or list a real
difference. Publish throughput recorded per layer as now (`artifacts/s`, `members/s`).

## 2. The driver holds `points.parquet` whole

**Problem.** The ingest cycle's hold-out is streamed from the rung's `points.parquet` through
Arrow and reaches 38 GB of RSS on rung 4 (52 GB file) by ~900 batches; both attribution runs
stalled at the same row count. Rung 5's `build_ids` had the same shape (fixed by streaming per row
group). It is a harness defect, not the write path's, and it is what stops rung 4's cell completing.

**Evidence.** `probes/2026-09-04-ingest-executor/README.md` (the rung 4 section and the
`runs/paperseek-92m-*` traces).

**Done looks like.** The hold-out read is bounded — row groups or `iter_batches`, one batch's
Arrow body live at a time — and rung 4's 10% cell runs to the fold on this box with the driver
under a few GB. Record the driver's peak beside the cell's numbers.

## 3. The flush's cost at a large base

**Problem.** After the executor fixes the binding term on rung 4 is the flush: 25 publications
over 8.83M rows into a 91.9M base is B/W ≈ 38 against the row trigger's 4, because a flush at that
base takes nine times the trigger's period; one window reached 8.76 s. Nothing yet attributes the
flush's own stages at scale.

**Evidence.** `probes/2026-09-04-ingest-executor/README.md` §"What is left"; the executor's laps
on `/control/status` (`write_executor.stage_nanos`, a `bench-timing` build); write-path §4 for
the flush's stages (plan, execute on the pool, publication by rebase on the executor).

**Done looks like.** The flush's wall partitioned by stage at 36M and 92M with the row trigger in
force, the term named with a number, and a fix or a memo — in that order, as the executor's was.

## 4. Labels containing commas on the wire

**Problem.** The driver encodes a row's `access` as a comma-separated descriptor list; 70 of
TreeOfLife's 474 publisher names contain a comma, so 148 fragment terms were minted and rows whose
fragment is itself a key landed in another compartment (`Natural History Museum, Vienna` →
`Natural History Museum`, 73,212 rows). Disclosure-shaped. Whether the label grammar gains an
escape or the wire carries a list is the owner's ruling; the build side is unaffected (it reads a
list column).

**Evidence.** `test_corpora/treeoflife/README.md` (the comma paragraphs), the rung's
`measurements.json` ingest block, `encode_batch` in the driver, the passthrough plugin's
`terms_of_label`.

**Done looks like.** A ruling, the encoding changed on whichever side it names, and rung 5's 50%
cell re-run reaching 233,055,986 visible under the declared terms alone.

## 5. Smaller items

- **The 25% and 50% MedCPT ingest cells** on the fixed engine: one command each now
  (`ingest_cycle.py --fraction`), ~40 min each; the spectrum row in the table is still 10% only.
- **Block-parallel compression in `RecordBlobWriter`.** The prose-extents change compresses on
  the join's single lane per text column; at 10⁷ uncapped that is +38 s on the join. Measured in
  `docs/design/build-column-extents.md` §8.
- **An indexed unique key's cost.** Rung 5's `uuid` keyword index is 8 GB of a 40 GB bundle. A rule
  or a warning at `tessera check` when an indexed keyword's cardinality is the row count.
- **A stale base is not refused at open.** A bundle built by an older engine 404s every artifact
  publish ("names nothing this deployment holds") instead of refusing at open. Found reconciling
  the driver merge; one sentence of design and a check.
- **Rung 3's abstracts ruling** (`ingest-campaign.md` §8): the memory objection is gone; the
  ruling turns on a 27.7 GB bundle and double the wall.
- **`perf` is not installed** on the box; `perf_event_paranoid` is 2, so it would work. Every
  attribution so far is by laps.
- A load-sensitive test, `a_stalled_or_disconnected_stream_is_shed_and_the_gauge_returns_to_zero`,
  fails on main when a fold or a battery shares the box and passes idle.

## How work is done here

One agent per track in a worktree off `main`, a written brief, a referee before merge
(`docs/agents/`). Every measured run waits for a quiet box (`pgrep -af "tessera (build|serve)"`);
long jobs under `setsid`; kill by pid only. Ports 8111–8153 have been used by sessions on this
box; take a fresh range. The campaign's per-rung numbers go through `measurements.json` and
`scripts/campaign_report.py`, never typed into the tracker.
