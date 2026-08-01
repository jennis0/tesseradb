# F1: viewport selection is O(Σvisible), not O(k) (2026-07-30)

Handoff note. Self-contained — assumes no shared context.

## The defect

`crates/tessera-engine/src/viewport.rs::sample_tile` wants `k` rows per tile. It gets them by
asking `EffectiveMask::iter_range` for the tile's visible rows and breaking after `k`:

```rust
let selected = mask.iter_range(range);
let mut remaining = k;
for row in selected {
    if remaining == 0 { break; }
    out.push(row_to_point(segment, row, declared_scalars));
    remaining -= 1;
}
```

But `iter_range` (`crates/tessera-engine/src/compose.rs:91-100`) is **eager**. It materialises
everything before the first `next()`:

```rust
pub fn iter_range(&self, r: Range<u32>) -> impl ExactSizeIterator<Item = u32> + '_ {
    let range_mask = Bitmap::from_range(r.clone());
    let mut result = self.base.bitmap().and(&range_mask);
    result.andnot_inplace(&self.minus);
    let plus_in_range = self.plus.and(&range_mask);
    result.or_inplace(&plus_in_range);
    result.to_vec().into_iter()      // <-- every visible row in the range, into a Vec<u32>
}
```

So a viewport's selection cost is **O(Σvisible), not O(tiles × k)**. The `break` saves the
*gather* but not the *selection*: the `Vec<u32>` is already built and populated by then.

## Evidence

Confirmed three ways.

**Unit.** `crates/tessera-engine/tests/viewport.rs::f1_selection_materialises_every_visible_row_not_k_of_them`
(needs `--features bench-timing`). At zoom 0 over the 10k-item fixture with `k=5`: 10,000 rows
materialised, 5 returned.

**Over the wire.** `crates/tessera-server/tests/http.rs::stage_timing_header_respects_the_compile_gate_and_carries_no_identifier`
asserts the same 10,000:5 through `POST /v1/viewport`.

**In the stage breakdown**, 2.42M `categories-subclass`, 5% coverage, k=30, zoom 8 — percentages
of total request time:

| Σvisible | tile_ranges | count | select | gather | materialised | returned |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 64.2% | 26.7% | 0.9% | 0.1% | 1 | 1 |
| 140 | 36.2% | 38.1% | 15.1% | 6.0% | 140 | 140 |
| 1,612 | 26.2% | 17.3% | 30.5% | 22.9% | 1,612 | 1,114 |
| 19,571 | 4.7% | 73.1% | 11.0% | 10.7% | 19,571 | 4,076 |

`materialised` tracks Σvisible exactly at every density.

**This is almost certainly the tail** `docs/evidence/memos/2026-07-30-tail-attribution.md` chased and
could not attribute. That memo measured, at k=50 over the 10⁹ bundle, latency correlating with
Σvisible at **r = 0.83** and with points-returned at **r = 0.008**, and normalised cost of
~0.00046 µs/row-visible (≈2.2 GB/s — memory-bandwidth shaped). At its mean (Σvisible 25.4M over
264 tiles) this code allocates and fills roughly **100 MB of `Vec<u32>` per request**; at its
observed max (144.8M) roughly **580 MB**. It concluded "gather/serialise/allocation path, still
unattributed to a specific phase" because no instrumentation existed. It does now.

## Two separable wins — please keep them separate

**Win 1 — stop materialising. Safe, immediate, no design dependency.**

Replace the eager `to_vec()` with streaming iteration. croaring 2.7 (already the workspace
dependency) has what is needed:

- `BitmapIterator::reset_at_or_after(u32)` — seek to the range start without walking to it
- `BitmapIterator::next_many(&mut [u32]) -> usize` — batched read into a caller-supplied buffer,
  the `roaring_bitmap_range_uint32_array` equivalent that architecture design §10.4 prescribes
- `Bitmap::rank(x)` / `Bitmap::select(position)` — §10.4's positioned access

For today's placeholder sampler this is O(k) visits and zero allocation. For *any* future sampler
it is at worst O(Σvisible) visits with O(k) space instead of O(Σvisible) space.

The wrinkle: `compose` currently produces `minus`/`plus` diffs against a base projection, and
`iter_range` composes them by building the intersection eagerly. A streaming merge of
`(base ∩ r) ∖ minus ∪ (plus ∩ r)` needs a three-way merge over three iterators rather than three
bitmap ops. With an empty overlay and buffer — the steady state, and what `compose_ns` measures at
~0.1% of a request — `minus` and `plus` are both empty and the merge degenerates to iterating
`base`, so the fast path is worth special-casing.

**Win 2 — stop *visiting* Σvisible at all. Needs the route chooser; probably not your change.**

Reducing visits below Σvisible is the CL/MD/SS selection-route work in
`docs/archive/plans/2026-07-30-selection-route-chooser.md`. Win 1 does not block it and does
not pre-empt it.

## The trap: do not bake in the prefix assumption

`sample_tile` is a **deliberate placeholder** and its own doc comment says so — it takes the first
`k` rows in storage order, which is *not* the I7 sampling definition. The real sampler takes the
`k` **lowest-priority** visible rows, where `priority = high16(tessera_id)` and `tessera_id` is a
keyed Feistel of the entity id — so priority is **uncorrelated with row order by construction**.

A fix shaped as `take_first_k_in_range(range, k)` is correct only for the placeholder and becomes
wrong the moment the real sampler lands, because the real selection is not a prefix. Shape the new
API as a **streaming range iterator** and leave the selection policy on top of it. A priority
sampler over that iterator is a bounded max-heap of size `k` — O(Σvisible) visits, O(k) space,
still a large win over today's O(Σvisible) space.

I am told the real priority work is in flight separately. Win 1 is deliberately independent of it.

## What breaks when you fix it, and what to do

1. **The canary test will fail.** `f1_selection_materialises_every_visible_row_not_k_of_them`
   asserts `select_rows_materialised == N_ITEMS` as an equality, on purpose, so the defect cannot
   drift unnoticed in either direction. Its failure message says what to change it to
   (`<= tiles_nonempty * k`). Please update it rather than delete it — and update the matching
   assertion in the HTTP test.

2. **`iter_range` returns `impl ExactSizeIterator`,** which I changed it to specifically so the
   instrumentation counter is free (`.len()` is O(1) on an already-materialised `Vec`). Streaming
   loses that guarantee. `StageTimings::select_rows_materialised` will need to be incremented by
   the sampler instead — please keep the counter alive, it is the only thing that detects a
   regression here. It lives in `crates/tessera-engine/src/timing.rs`.

3. **`iter_range` has a second caller that genuinely wants full iteration:**
   `crates/tessera-engine/tests/compose.rs:405` uses `mask.iter_range(r).count()` as the
   brute-force oracle for `count_range`. Keep a full-iteration path for it, or give the oracle its
   own. Do not make the oracle depend on the thing it is checking.

## How to measure the fix

Build with `--features bench-timing`; it is off by default and adds nothing when off (the gate is
on the clock type, not the call sites). Server-side emission additionally needs
`[serve] stage_timing = true`; both gates are asserted by `scripts/check-layers.sh` and a
conformance test.

```
cargo build --release -p tessera-bench
./target/release/tessera-bench viewport \
    --scale 2422486 --label-set categories-subclass \
    --mode battery --k 30 --coverage 0.05 --zoom 8 --repeat 7 \
    --run-dir /tmp/f1-after
```

Fixtures are prebuilt at `/tmp/tessera-bench/fixtures/<scale>/<label-set>/` for scales
250,000 / 2,422,486 / 25,000,000; rebuild with `scripts/bench_build_fixtures.sh` (idempotent).

Each JSONL record carries `stages` (per-stage ns), `work.rows_materialised`,
`work.points_gathered`, and a `selection_overdraw=<n>x` flag when materialised exceeds returned by
more than 4×. The `battery` mode probes the corpus for density deciles first — a uniform-random
window at zoom 8 usually contains nothing, because the geometry is UMAP output and heavily
concentrated, so an unprobed "battery" measures the cost of finding nothing ten times.

**Expected after Win 1:** `select_rows_materialised` drops to ≈ `points_gathered`; `select_ns`
share collapses; `unattributed_ns` (allocation) drops with it. `count_ns` should be **unchanged** —
if it moves, something else changed too.

## Two adjacent findings, not yours to fix, but context

- **`tile_ranges` is 26–64% of a sparse viewport.** The binary search over `morton.u32` dominates
  low-density requests. Previously invisible; nobody has looked at it.
- **F2: `compose` iterates the entire overlay and buffer on every viewport**
  (`compose.rs:183` and `:200`, with a `perm.row_of` per entry), so viewport latency is
  O(|overlay| + |buffer|) against `overlay_soft_limit = 500_000`. It measures ~0.1% here only
  because these fixtures have an empty overlay. It will bite under sustained ingest.
