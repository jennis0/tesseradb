# P4 — raw output

**Host:** AMD Ryzen 9 5900X (12 cores), 47 GiB RAM, WSL2 6.18.33.2, `/dev/sdd` (12 GiB swap,
disabled inside the scope). Release build, 2026-08-07.

**Harness:** `crates/tessera-engine/tests/scale.rs`,
`a_live_viewport_against_a_real_fold_with_and_without_the_advice`, `#[ignore]`d.

The analysis is `docs/evidence/memos/2026-08-07-fold-against-a-live-viewport.md`. What is here is
what the runs printed.

---

## How the regime was reached

A fold doubles disc, so a bundle larger than this machine's 47 GiB of RAM would need ~96 GiB free to
fold and there is not that much. The regime therefore comes from a **cgroup**, which charges page
cache and reclaims against `memory.max`:

```text
systemd-run --user --scope -q -p MemoryMax=3G -p MemorySwapMax=0 -- \
  env TESSERA_P4_BUNDLE=<prebuilt> TESSERA_P4_BASE=100000000 \
      TESSERA_P4_ROUNDS=2 TESSERA_P4_BATCH=500000 \
  ./target/release/deps/scale-<hash> \
  --exact a_live_viewport_against_a_real_fold_with_and_without_the_advice \
  --ignored --nocapture --test-threads=1
```

3.81 GiB bundle against a 3.00 GiB cap: **EVICTING**, which the probe asserts nothing about and
prints in every run.

**The bundle is built outside the cap and copied in per arm.** A build *inside* a tight cap is
killed — the write-heavy phase fills the limit with its own output's page cache. Measured: an
80M-row build inside 3 GiB died before publishing.

---

## Pair 1 — `on` then `off`

⚠ **Contaminated, and kept for that reason.** A full `cargo test --workspace` ran on the same
machine during this pair. It is reported because discarding a run silently is how a campaign comes
to quote only the numbers it liked.

```text
P4: page cache bound 3.00 GiB (cgroup memory.max)

MADV_SEQUENTIAL=on
  bundle 3.81 GiB | page cache 3.00 GiB (cgroup memory.max) ⇒ EVICTING
  fold 561.2s, 756 sweeps inside it
  quiet (before):    z0(1t) 54.8ms  z2(16t) 57.2ms  z4(256t) 68.3ms  z6(2362t) 56.3ms  z8(2972t) 66.4ms
  during the fold:   z0 57.9ms [1.06x]  z2 60.9ms [1.06x]  z4 71.3ms [1.04x]  z6 69.1ms [1.23x]  z8 97.2ms [1.46x]
  quiet (after):     z0 52.8ms [0.96x]  z2 55.4ms [0.97x]  z4 70.0ms [1.03x]  z6 51.6ms [0.92x]  z8 63.1ms [0.95x]

MADV_SEQUENTIAL=off
  fold 948.8s, 1384 sweeps inside it
  quiet (before):    z0 54.4ms  z2 56.6ms  z4 67.2ms  z6 51.2ms  z8 65.4ms
  during the fold:   z0 61.1ms [1.12x]  z2 61.4ms [1.09x]  z4 72.1ms [1.07x]  z6 58.7ms [1.15x]  z8 75.9ms [1.16x]
  quiet (after):     z0 54.1ms [1.00x]  z2 55.9ms [0.99x]  z4 68.1ms [1.01x]  z6 62.9ms [1.23x]  z8 61.5ms [0.94x]
```

## Pair 2 — `off` then `on`, order reversed, machine otherwise idle

```text
MADV_SEQUENTIAL=off
  fold 2656.1s, 3862 sweeps inside it
  quiet (before):    z0 54.6ms  z2 57.0ms  z4 71.9ms  z6 50.6ms  z8 65.8ms
  during the fold:   z0 59.1ms [1.08x]  z2 61.3ms [1.08x]  z4 72.7ms [1.01x]  z6 59.4ms [1.17x]  z8 77.5ms [1.18x]
  quiet (after):     z0 55.4ms [1.01x]  z2 54.9ms [0.96x]  z4 69.1ms [0.96x]  z6 51.8ms [1.02x]  z8 63.2ms [0.96x]

MADV_SEQUENTIAL=on
  fold 2443.7s, 3623 sweeps inside it
  quiet (before):    z0 55.3ms  z2 62.0ms  z4 66.6ms  z6 51.8ms  z8 72.4ms
  during the fold:   z0 58.9ms [1.07x]  z2 59.7ms [0.96x]  z4 71.5ms [1.07x]  z6 57.9ms [1.12x]  z8 75.9ms [1.05x]
  quiet (after):     z0 54.3ms [0.98x]  z2 56.1ms [0.90x]  z4 71.2ms [1.07x]  z6 54.7ms [1.05x]  z8 64.2ms [0.89x]
```

---

## The four runs beside each other

| pair | arm | fold | sweeps | sweeps/s | z6 | z8 | quiet-after drift |
|---|---|---|---|---|---|---|---|
| 1 ⚠ | on | 561 s | 756 | 1.35 | 1.23× | **1.46×** | −8% … +3% |
| 1 ⚠ | off | 949 s | 1384 | 1.46 | 1.15× | 1.16× | −6% … **+23%** |
| 2 | off | 2656 s | 3862 | 1.45 | 1.17× | 1.18× | −4% … +2% |
| 2 | on | 2444 s | 3623 | 1.48 | 1.12× | **1.05×** | −11% … +7% |

**Fold wall clock varied 4.7× at identical configuration** (561 s to 2656 s). Whatever drives that
— device state, free space, the host's own cache outside the cgroup — it is larger than any
difference between the arms, so **no cross-pair comparison of duration means anything**. Within a
pair, `on` was faster both times (41% and 8%), by margins that do not agree with each other.

**The viewport ratios disagree about sign.** At z8, pair 1 says `on` is worse (1.46 vs 1.16) and
pair 2 says `on` is better (1.05 vs 1.18). The differences — 26% and 12% — are the size of the
`quiet-after` drift in the same runs.

The sweep *rate* is the one stable figure: 1.35–1.48 per second across all four.
