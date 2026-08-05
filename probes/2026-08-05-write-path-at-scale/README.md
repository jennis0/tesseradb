# Write path at scale — 2026-08-05

Raw output of `crates/tessera-engine/tests/scale.rs` at two configurations. Findings and what they
mean: [`docs/evidence/memos/2026-08-05-write-path-at-scale.md`](../../docs/evidence/memos/2026-08-05-write-path-at-scale.md).

**This is not a script — the campaign is a test in the tree**, which is the point: every figure
below is reproduced by running the suite, not by resurrecting a probe binary that has since drifted
from the engine it measured.

```bash
# defaults: 1,000,000 base + 16 × 250,000 = 5,000,000
cargo test -p tessera-engine --release --test scale -- --ignored --nocapture

# large: 2,000,000 base + 32 × 250,000 = 10,000,000
TESSERA_SCALE_BASE=2000000 TESSERA_SCALE_ROUNDS=32 TESSERA_SCALE_BATCH=250000 \
  cargo test -p tessera-engine --release --test scale -- --ignored --nocapture
```

Both are in [`runs.txt`](runs.txt), in that order.

## Machine

WSL2 on Linux 6.18, 12 cores, 39 GB RAM, NVMe. **Not a latency-certification environment** — the
Phase 0 memo's standing caveat about WSL2 applies to every wall-clock number here. What the figures
are good for is *shape*: how a quantity moves as the corpus grows, and whether a per-unit cost
stays flat. Absolute milliseconds are indicative.

## Reading a round

Each round prints two lines:

```
round 13: ack 4.3s | publish 527.1ms | refresh 76.0ms | visible=4500000 segments=6 deltas=7 merges=3 coalesces=1
          disc 276.2 MiB (live 128.0 MiB, 2.16x orphan) | zoom z0(1t) 2.5ms [418.0us/ts] ...
```

| field | meaning |
|---|---|
| `ack` | every `accept_ingest` in the round: WAL append + fsync + buffer swap. Rows are **durable and invisible** at the end of it |
| `publish` | the forced flush: buffered rows become a segment and a generation is published |
| `refresh` | the background refresh brings live sessions' row projections forward |
| `visible` | masked total from a whole-extent viewport — the assertion, not a gauge |
| `segments` / `deltas` | the two axes the merge and the coalesce respectively bound |
| `disc` / `live` | bytes under the bundle root, against bytes the live manifest still names |
| `zN(Kt) T [U/ts]` | zoom N covered K occupied tiles in T; U = microseconds per (tile × segment) |

`ack` is **not** fsync-dominated, though an earlier revision of this file said it was: 25 sub-batches
of 10,000 at the measured ~3.2 ms floor is ~80 ms of a 1.8–5.0 s round, **2–4%**. It is a property of
the caller's batch size rather than of the corpus — see
`crates/tessera-engine/tests/ingest_shape.rs`, which splits it, and the memo's ingest section for
what is and is not attributed.
