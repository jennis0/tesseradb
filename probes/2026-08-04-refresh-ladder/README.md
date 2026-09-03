# P1 / P2 — the refresh ladder and the fragment build, measured

**Date:** 2026-08-04 · **Tool:** `crates/tessera-engine/examples/refresh_probe.rs` · **Raw:**
`1e9.txt`, `1e8.txt`

The two measurements decision
0044 requires
before its refresh mechanism is coded. Machine: 12 threads, 47 GB.

Re-run:

```
cargo run --release --example refresh_probe -p tessera-engine -- [--entities N] [--grant F] [--dir PATH]
```

## Results at 10⁹ entities, 25% grant

| | median | max |
|---|---|---|
| full rebuild (`RowSpace::project`) | **4 550 ms** | — |
| bitmap clone (the patch's fixed cost) | **40.9 ms** | 128 ms |
| union over one new flush extent | **0.24 ms** | 0.41 ms |
| span rebase over a 4-extent merge | **44.6 ms** | 137 ms |
| fragment build, 0 / 1 / 8 / 64 / 512 tiers | **313 / 199 / 192 / 210 / 198 ms** | 431 / 295 / 211 / 248 / 205 ms |

The grant's projection serialises to **125.12 MB** — the same figure `probes/results.md` §4.2
measured on a real 10⁹ bundle, which is what says the synthetic row space has the right shape.

## What P1 decides

**The patch's cost is the clone, and nothing else.** 40.9 ms of a 41.1 ms patch. Decision 0044's
D1 called the inline patch "tens of milliseconds… two orders over the budget" from the 125.12 MB
entry size; that is now measured rather than inferred, and it holds: **40.9 ms against a < 0.2 ms
budget**. No arrangement of the inline patch reaches the budget, because a clone of the entry is
unavoidable in it — the cached value is immutable (lifecycle §7), so a patch must copy before it
unions.

**The span rebase is affordable as background work: 44.6 ms per resident entry**, 102× cheaper
than the 4 550 ms rebuild it replaces. At the ~16 wide-grant entries a 2 GiB cache bound holds,
one refresh round is **~0.7 s of pool time** — which is what sizes 0044's 429 residual window, and
it is comfortably "much rarer than the flush/merge rate" for any merge cadence.

**One defect found and fixed by this probe.** `SegmentExtent::project` walked the mask from its
*start*, skipping up to `entity_lo` — so projecting one flush extent cost O(grant cardinality)
rather than O(extent span). Measured 79 ms per extent at 10⁸ before, 0.199 ms after
(`reset_at_or_after`); at 10⁹ the union is 0.24 ms. Every rung of the ladder improved with it —
the rebuild by 2.1× at 10⁸, the span rebase by 160× — because all three walk the same code. The
patch's cost was a function of the *grant's width* rather than of the flush's size, which is the
opposite of what the design claims for it.

## What P2 decides — a negative result

**The fragment build does not grow with tier count, and it is not seconds-scale.** 199 ms at one
tier, 198 ms at 512. The write-path design carried it as *"unmeasured, modelled seconds — the
largest unpriced request-thread term"*; the model was wrong. The cost is the base posting union
over the credential's satisfied terms, which is fixed, and a tier probe is a binary search over a
few hundred terms.

Two consequences, both stated so they are not re-derived:

- **§11.2's incremental fragment form (`old ∪ (delta ∩ satisfied)`) does not land before Task
  22b.** Decision 0044's D4 made that conditional on the build being seconds-scale at 10⁹; it is
  200 ms. The incremental form would replace a 200 ms build with a ~41 ms clone — real, but a
  factor of five on work that has to move off the request thread anyway, where the mechanism that
  moves it is the same background refresh the projection needs. It buys nothing the refresh does
  not already buy.
- **200 ms is still three orders over D1's budget**, so the fragment must be refreshed in the
  background beside the projection — not left inline because it turned out cheaper than modelled.
  This measurement moves it from *"the largest term, unpriced"* to *"a bounded term, priced"*; it
  does not make it conformant.

## Limitations, stated

Synthetic. The probe writes a `permutation.bin` and a postings file directly and measures the
primitives at the shape a wide grant produces — not an end-to-end request, and nothing here
measures bundle IO, cache residency or contention. The absolute rebuild figure is therefore
**4.55 s where the corpus's measured end-to-end figure is 10.7 s** (`probes/2026-07-30-1e9-rebuild/`);
the ratios between the rungs are what this probe is for, and those are faithful because every rung
walks the same slot array and the same bitmap. Quote 10.7 s for the rebuild; quote these numbers
for the ladder.
