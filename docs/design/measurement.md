# Measurement — design

**Date:** 2026-08-02
**Status:** Provisional — under review, and **not approved**. **To become normative:** owner
sign-off, the §10 amendments landed in `bench/README.md` and `bench/matrix.toml`, and the schema-2
`Work` fields carried by at least one arm — the two reporting modes (§4) are the part a reader could
otherwise take as already in force.
**Reads against:** architecture §4 (I2, I7), §7.2, Appendix A, Appendix C (C4); conformance §6;
`flush-and-merge.md` §14; `per-point-attributes.md` §8; `hot-row-geometry.md` §7;
`probes/optimisations.md` §0; `probes/results.md` §1, §5, §6.
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **spec §n**.

**Owns:** what the benchmark suite measures and why — the axes, the denominators, the reporting
conventions, and which figures may be published. **Does not own:** how to run it (that is
[`bench/README.md`](../../bench/README.md)), what any particular run found (the arms' module docs
and `docs/evidence/memos/`), or the correctness gate (`conformance.md`).

---

## 1. Summary

The suite measures a **frozen bundle under a varying request**. Flush, merge and per-point
attributes each break that assumption from a different side: the first two make the bundle move
under a running reader, the third makes the row wider. The expansion is therefore not a longer list
of arms but **one new axis** — the bundle's own state — carried in the record beside the container
counts that already make a latency portable.

Two conventions follow from it, and they are the part most easily got wrong:

- `min_ns` over 3–5 repetitions is correct for A/B of a code path and **cannot express a
  percentile**. A merge is a scheduled perturbation on a minority of requests; min-of-N is
  constructed to absorb exactly that.
- Throughput has no single unit here. Requests, served marks and masked rows examined scale on
  three different denominators, and the middle term — the masking arithmetic, which is the product —
  is invisible in the two units a reader reaches for first.

---

## 2. The state axis

Every arm today holds the bundle constant and varies the request. Nothing varies the bundle under a
constant request, because until flush there was nothing to vary: `tessera build` produced one
segment and it stayed.

After flush, a reader's cost depends on quantities no existing record carries. A tile resolves to
one contiguous range **per live segment** (§11.3) and a fragment build unions across **every live
delta postings tier**, so a viewport's latency is a function of the merge policy's recent history.
A regression and "the merge policy left more segments live" are the same number without those
counts beside it, and they are the most likely false alarm of the next campaign.

This is the identical argument [`report.rs`](../../crates/tessera-bench/src/report.rs) already makes
for `containers`, applied to the write path. It is why `work` is not an `Option` there, and the new
fields join it on the same terms.

### 2.1 `Work` gains the write-path denominators

Schema bumps to **2** — a field's meaning does not change, but a reader that assumes `segments` is
absent rather than zero would misread every pre-flush record.

| field | what it explains |
|---|---|
| `segments` | ranges resolved per tile; the merge policy's whole justification |
| `delta_tiers` | operands in a fragment build |
| `buffered_items` | `compose_ns` — F2's measured denominator |
| `overlay_entries` | `compose_ns`'s other half, and the deny path's depth |
| `hot_row_bytes` | bytes per served mark, once the tail is variable (spec §5) |
| `attribute_columns` | per-point-attributes §3.6's *assumed* claim that code width is free |

`ack_to_visible_ns` is **not** a `Work` field. It is a latency, and it belongs in `timing` on the
arms that can observe it.

### 2.2 The soak arm

One new arm shape carries the axis: continuous ingest at a configured rate for hours, a reader
drawing a fixed density battery throughout, reporting **time-bucketed** read latency alongside the
§2.1 counts, ack→visible, and RSS.

It is the only place several of flush-and-merge §14's obligations are observable at all — §14.10
(segment **and** delta-tier count bounded under sustained ingest) and §14.11 (the ack→visibility gap
bounded by `flush_max_age_secs`) are soak properties by construction, and a unit test cannot see
them. §14.13 and §14.18 fit here for the same reason.

> ⊘ **The arm is buildable before flush is; the properties are not.** Against today's engine it
> correctly reports a flat read latency and an unbounded ack→visible gap, because there is no flush.
> That is a useful negative baseline and it is not a partial pass — the gates in spec §8 stay
> unarmed until there is a flush to bound.

---

## 3. What the existing ingest arms stop measuring

[`bench/README.md`](../../bench/README.md) §9 records the caveat that governs them: *"Ingested rows
never become visible."* Flush ends that condition, and two arms are re-derived rather than extended.

`ingest-continuous` currently measures `compose` over **rejected** entries — a buffered entity has
no row in the segment permutation, so `compose` skips it at `perm.row_of`. F2's measured ~10 ns per
buffered item is therefore a **lower bound over the cheap branch**. Resolved entries additionally
push into `pass_rows`/`fail_rows` and build the diff bitmaps. Re-baseline the constant; do not
inherit it.

F3 remains **NOT confirmed by measurement**, and the reason is unchanged: a single-item ack is
~3.2 ms and entirely fsync-dominated, so any O(buffer) clone term is buried. An attribute-tail sweep
that stops at batch=1000 will report attribute cost as free and be wrong for the same reason.

---

## 4. Two reporting modes

`Timing::from_samples` computes `p99` by nearest rank. At `repeat = 5` — [`matrix.toml`](../../bench/matrix.toml)'s
setting for `viewport` and `gather` — that resolves to the last index, so **`p99_ns` is exactly
`max_ns` in every in-process record the suite has produced.** Gate G2 (battery p99−p50 within +15%)
is consequently gating on max-minus-median of five samples: it catches a gross stall and cannot see
a tail.

That is not a defect in the convention. `probes/results.md` §6's min-of-N was chosen to absorb
scheduler noise and page-cache warmth without a warm-up phase, and it does. It is the wrong
convention for a percentile, and the fix is to name the two modes rather than to raise `repeat`
everywhere — the matrix is ~5,600 cells against a possible ~27,000 and that trim is well argued.

**A/B mode** — unchanged. `min_ns` headline, `repeat` 3–5, normalised per container / row-visible /
tile / mark. This is what gates, and what compares two implementations.

**Distribution mode** — duration-bounded (`--duration`) rather than repetition-bounded, collecting
N ≥ 1,000 per cell. Applies to the arms whose output is user-facing or tail-related, at the declared
operating points only. `load` is already in this mode in all but name: it issues hundreds of
thousands of requests per cell and computes percentiles over the complete set before thinning to
`MAX_RETAINED_SAMPLES`.

**Below 100 samples, `p99_ns` is suppressed rather than emitted.** A number that reads as a
percentile and is a maximum is worse than an absent field. This is the same discipline as the
existing `low_container_resolution` flag, which already refuses to report `ns_per_container` below
32 containers.

The soak arm inherits distribution mode by construction, and reports over buckets rather than over a
cell, with flush and merge publications marked on the timeline.

---

## 5. What throughput means here

A viewport's cost has three terms, on three denominators:

| term | scales with | visible in |
|---|---|---|
| per request | — | requests/s |
| **masking** — count pass, compose, projection | `rows_in_ranges`, `sigma_visible`, containers | **neither** |
| output | `points_gathered`, bytes | marks/s, MB/s |

The middle term dominates, and both units a reader reaches for first are blind to it. Arithmetic
from [`load.rs`](../../crates/tessera-bench/src/arms/load.rs)'s recorded Arm B figures — 48,588 rps
at k=30 — puts the server at ~1.46 M served marks/s and, at the points batch's per-point width, tens
of MB/s, while burning 8.2 of 12 cores. Neither rate is near a bottleneck. **Arithmetic, not a
measurement**, and quoted only to locate the cost.

**MB/s served is rejected as a headline, and not merely as uninformative.** It inverts the
incentive: a viewport that counts a billion masked rows and returns thirty marks is the system
working, and by bytes served it is the worst cell in the run — while a degenerate 100%-coverage
principal is the best. Bytes/mark is also a constant today, so the figure is marks/s times a
constant and carries no information at all. It becomes load-bearing exactly when the attribute tail
lands, and then as a **saturation check** — has the serialiser or the wire become the bottleneck —
rather than as a performance metric.

**Masked rows examined per second** is the unit the other two lack. `rows_in_ranges` and
`sigma_visible` are the throughput of the masking arithmetic, and the only unit that does not
collapse when selectivity changes.

### 5.1 The load arm's `work` is a stub

It fills `points_gathered` and `bytes_touched` and leaves the rest at `Default`. The tile stream it
already decodes carries `visible`, `matched` and `served` per tile, and it sums `served` alone.
Summing all three is near-free and gives the arm a real `work` block — without which no cell it
produces can be normalised the way every other arm's can.

---

## 6. The published figures

Six, and the constraint on them matters more than the list.

1. **Ack→visible, p99.** "A write is queryable within X." The strongest external consequence of the
   flush work: it turns `flush_max_age_secs` from a config key into a claim.
2. **Points/second ingested, against concurrent writers.** Must be **over the wire**
   (`/control/ingest`). `ingest-concurrent` is in-process, so it excludes serialisation, the handler
   and the queue — a gap that is invisible in the number and unacceptable in a published one.
3. **Requests/second at a stated SLO**, both session arms. Not peak: Arm A at c=1000 sustained
   18,599 rps with a **1.04 s p99** (the F4 projection-lock finding), which a peak-throughput
   headline reports as healthy.
4. **Served marks/second**, which survives a change in `k` where requests/second does not.
5. **Latency against corpus scale** at a fixed viewport, k and coverage held.
6. **Latency against points-in-view**, never against viewport area — the geometry is UMAP output and
   heavily concentrated, so area is not a workload.

**Every one carries its coverage.** A figure taken at 100% coverage measures the absence of masking;
the suite already flags such a cell `degenerate`, but a rendered plot does not inherit a flag.
Whatever renders these must **refuse** a `degenerate` or `generator_bound` cell rather than footnote
it. Appendix C's C4 is the reason this is a design rule and not a presentation preference: coverage
is the axis along which a timing channel is quantified, so publishing latency without it is
publishing half of C4.

---

## 7. Arms

**New.**

- **`soak`** (spec §2.2).
- **`filter`** — vocabulary-filter cost against vocabulary size × **principal sparsity** × overlay
  depth, at cold and cached fingerprints. per-point-attributes §3.3 predicts sparse principals are
  cheapest; an arm that does not vary sparsity cannot check the prediction it most needs to. Second
  output: latency against filter selectivity, where I3/I12 give a checkable expectation — a filter
  may move the frontier up, never down.
- **`ingest-wire`** — `load`'s generator pointed at `/control/ingest`, open and closed loop, reusing
  its `/healthz` ceiling calibration and `generator_bound` flag.

**Extended.**

- `gather`, `viewport` — attribute-tail column axis; bytes per served mark split geometry ÷ tail;
  varying the **number** of category columns, not only their width.
- `ingest-build` — an attribute-column stage in the eleven-stage decomposition.
- `ingest-batch` — swept past 1000, per spec §3.
- `ingest-continuous` — re-derived, per spec §3.

**Infrastructure**, and the cheapest items here.

- **A `variant` field on `Record`**, so two implementations write into one run directory and
  `bench_collate.py` pairs cells. Today an A/B is two run directories and a hand diff.
  hot-row-geometry §7 hit this exactly — *"measuring this shape against the previous one needs a
  second column source and a build able to emit both"*. Generalise once rather than per comparison.
- **A fixture attribute-tail knob** in `bench_build_fixtures.sh` (width and type mix, declared and
  discovered). per-point-attributes §8 opens by saying no arm can see any of this without it; it
  blocks the whole attributes half.

---

## 8. Gates

G0–G3 are unchanged. Two are added, and both are **soak-only** — per-PR cannot afford hours, so
they belong to conformance §6's nightly tier, which does not exist (⊘).

| gate | checks | on failure |
|---|---|---|
| **G4** | soak p99 shows no monotonic drift across the window beyond +15% | steady-state regression — the class a frozen-bundle suite structurally cannot catch |
| **G5** | segment count, delta-tier count and ack→visible within configured bounds | flush-and-merge §14.10/§14.11 violated |

G5 is an assertion about the system, not about its speed, and it fails the run rather than reporting
a regression.

---

## 9. Sequencing

The `variant` field, the load arm's `work` block and the `p99` suppression are unblocked now, and
the first two block comparisons the other work will want. The fixture knob blocks every attributes
arm. The soak **harness** is independent of flush; its **gates** are not (spec §2.2).

The state axis is expensive in wall clock, not in cell count — one soak cell is hours. It is
declared outside `matrix.toml`, as `load` and `ingest-build` already are, for the reason that file
gives: putting an hours-long cell beside a two-microsecond one makes the default run useless.

---

## 10. Amendments

- **`bench/README.md` §8** — the two reporting modes beside the existing `min_ns` rationale, and the
  `p99` suppression rule; the §9 caveat *"Ingested rows never become visible"* retires when flush
  lands, and the arms it governs are re-derived rather than edited.
- **`bench/README.md` §7** — G4 and G5 in the gate table, marked nightly-tier.
- **`bench/matrix.toml`** — the attribute-tail axis on `gather` and `viewport`; `filter` declared;
  `soak` and `ingest-wire` noted as deliberately absent, with the reason.
- **`report.rs`** — `SCHEMA_VERSION` to 2 and the spec §2.1 fields.
- **`conformance.md` §6** — the nightly tier gains G4/G5 when it gains anything at all.

---

## 11. What this does not do

- **No new corpus.** Every axis here is constructible within the existing fixtures plus an attribute
  tail. Signature-sorted contiguity still cannot be synthesised and still costs a rebuild.
- **No memory-pressure mechanism.** hot-row-geometry §7's residency saving becomes latency only
  under contention, and the `cold_*` arms are a token-cache split, not a page-cache one. Unchanged
  here, and the saving stays arithmetic against Appendix A.
- **No published figure for anything unbuilt.** ⊘ Four of the six spec §6 figures cannot be produced
  today: ack→visible and the soak curve need flush; the attribute split needs the tail; the wire
  ingest number needs the arm.
- **No replacement for the correctness gate.** A soak that stays fast while leaking passes every
  gate here. `conformance.md` owns that, and this document does not weaken the split.

---

## Appendix R — review record

Not yet reviewed. Drafted 2026-08-02 from the benchmark obligations recorded in
`flush-and-merge.md` §14, `per-point-attributes.md` §8 and `hot-row-geometry.md` §7, plus two
findings from reading the harness: `p99_ns` is identically `max_ns` at the matrix's own repetition
counts (spec §4), and the load arm emits a stubbed `work` block despite already decoding the
columns that would fill it (spec §5.1).
