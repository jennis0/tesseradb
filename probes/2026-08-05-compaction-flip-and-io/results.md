# P2 and P3 — raw output

**Host:** AMD Ryzen 9 5900X (12 cores), 47 GiB RAM (44 GiB available), WSL2 6.18.33.2, `/dev/sdd`
at 96% (16 GiB free). Release build, 2026-08-05.

**Harness:** `crates/tessera-engine/tests/scale.rs`, both `#[ignore]`d.

```text
cargo test -p tessera-engine --release --test scale -- --ignored --nocapture <name>
```

The analysis is `docs/evidence/memos/2026-08-05-compaction-flip-and-io.md`. What is here is what the
runs printed.

---

## P2 — `the_flip_costs_what_the_resident_population_costs`

### 20M base, 2M ingested, 16 resident sessions

```text
P2: base=20000000 batch=2000000 sessions=16
  16 resident entries warmed
  derive: 386.706205ms over 16 entries = 24.169137ms/entry (the flush and merge path)
  cold:   351.552885ms/entry over 4 samples (what a fold forces, every entry)
  observed window: 0 shed, longest wait 56.342397ms, mean 36.923313ms
  ⇒ projected flip at 16 resident entries: 5.62484616s (last session), 2.81242308s (mean)
```

### 20M base, 2M ingested, 32 resident sessions

```text
P2: base=20000000 batch=2000000 sessions=32
  32 resident entries warmed
  derive: 725.090399ms over 32 entries = 22.659074ms/entry (the flush and merge path)
  cold:   337.375284ms/entry over 4 samples (what a fold forces, every entry)
  observed window: 0 shed, longest wait 79.652173ms, mean 40.647232ms
  ⇒ projected flip at 32 resident entries: 10.796009088s (last session), 5.398004544s (mean)
```

### 20M base, 1M ingested, 16 resident sessions

```text
P2: base=20000000 batch=1000000 sessions=16
  16 resident entries warmed
  derive: 177.776933ms over 16 entries = 11.111058ms/entry (the flush and merge path)
  cold:   266.761847ms/entry over 4 samples (what a fold forces, every entry)
  observed window: 0 shed, longest wait 30.867477ms, mean 22.486289ms
  ⇒ projected flip at 16 resident entries: 4.268189552s (last session), 2.134094776s (mean)
```

### 200k base, 50k ingested, 4 resident sessions — the small-deployment shape

```text
P2: base=200000 batch=50000 sessions=4
  4 resident entries warmed
  derive: 8.341965ms over 4 entries = 2.085491ms/entry (the flush and merge path)
  cold:   6.529208ms/entry over 4 samples (what a fold forces, every entry)
  observed window: 0 shed, longest wait 2.146796ms, mean 1.774225ms
  ⇒ projected flip at 4 resident entries: 26.116832ms (last session), 13.058416ms (mean)
```

---

## P3 — `a_streaming_read_of_the_whole_bundle_against_a_live_viewport`

### 1.2B base, 4 rounds of 2M — 45.57 GiB bundle against 36.88 GiB of RAM, **evicting, no cgroup**

The real regime: a bundle genuinely larger than the machine's available memory, so global reclaim
rather than cgroup reclaim. Bundle:cache is **1.24:1**. 833 s end to end, most of it the base build.

```text
P3: base=1200000000 rounds=4 batch=2000000
batching: 2 batches of <= 905969664 items (signature order is per-batch — §11.1 r23; recorded in provenance; a rebuild preserving this identity must replay it)
  bundle 45.57 GiB across 5 segments | page cache 36.88 GiB (MemAvailable) ⇒ EVICTING — the regime a fold actually creates
  quiet (before):   z0(1t) 667.7ms  z2(16t) 340.1ms  z4(256t) 215.3ms  z6(3172t) 270.4ms  z8(4932t) 320.9ms
  unthrottled:      z0(1t) 684.3ms [1.02x]  z2(16t) 420.8ms [1.24x]  z4(256t) 245.6ms [1.14x]  z6(3172t) 317.3ms [1.17x]  z8(4932t) 366.5ms [1.14x]
                    read 7.32 GiB in 4.1s (1821 MiB/s achieved)
  2048 MiB/s:       z0(1t) 651.9ms [0.98x]  z2(16t) 345.7ms [1.02x]  z4(256t) 223.3ms [1.04x]  z6(3172t) 265.9ms [0.98x]  z8(4932t) 306.8ms [0.96x]
                    read 7.50 GiB in 3.8s (2029 MiB/s achieved)
  1024 MiB/s:       z0(1t) 645.1ms [0.97x]  z2(16t) 322.4ms [0.95x]  z4(256t) 205.4ms [0.95x]  z6(3172t) 248.3ms [0.92x]  z8(4932t) 284.4ms [0.89x]
                    read 3.49 GiB in 3.5s (1024 MiB/s achieved)
  512 MiB/s:        z0(1t) 618.8ms [0.93x]  z2(16t) 333.6ms [0.98x]  z4(256t) 211.3ms [0.98x]  z6(3172t) 263.7ms [0.98x]  z8(4932t) 296.3ms [0.92x]
                    read 1.73 GiB in 3.5s (512 MiB/s achieved)
  128 MiB/s:        z0(1t) 622.3ms [0.93x]  z2(16t) 331.4ms [0.97x]  z4(256t) 207.5ms [0.96x]  z6(3172t) 255.6ms [0.95x]  z8(4932t) 291.0ms [0.91x]
                    read 0.44 GiB in 3.5s (128 MiB/s achieved)
  32 MiB/s:         z0(1t) 630.5ms [0.94x]  z2(16t) 328.7ms [0.97x]  z4(256t) 213.7ms [0.99x]  z6(3172t) 255.6ms [0.95x]  z8(4932t) 292.0ms [0.91x]
                    read 0.11 GiB in 3.5s (32 MiB/s achieved)
  quiet (after):    z0(1t) 628.7ms [0.94x]  z2(16t) 334.2ms [0.98x]  z4(256t) 210.0ms [0.98x]  z6(3172t) 259.0ms [0.96x]  z8(4932t) 296.9ms [0.93x]
```

**The internal control that bounds this run's confidence.** The unthrottled reader achieved
1 821 MiB/s and the 2 048 MiB/s-throttled one achieved 2 029 MiB/s — the *same* workload, the
throttle never binding — yet they measured 1.02–1.24× and 0.96–1.04× respectively. So run-to-run
noise on this configuration is of the same order as the unthrottled effect, which is why a second
sample was taken.

### The second sample, same configuration

The effect is clearer here and monotone in rate: unthrottled 1.25–2.03×, 2048 ≈1.2×, 1024 ≈1.05×,
128 and 32 at noise. One 1.48× at zoom 0 under 512 MiB/s — zoom 0 is the single-tile sweep and the
noisiest column in every run.

```text
P3: base=1200000000 rounds=4 batch=2000000
batching: 2 batches of <= 922746880 items (signature order is per-batch — §11.1 r23; recorded in provenance; a rebuild preserving this identity must replay it)
  bundle 45.57 GiB across 5 segments | page cache 38.20 GiB (MemAvailable) ⇒ EVICTING — the regime a fold actually creates
  quiet (before):   z0(1t) 633.5ms  z2(16t) 343.6ms  z4(256t) 205.7ms  z6(3172t) 248.1ms  z8(4932t) 298.1ms
  unthrottled:      z0(1t) 789.7ms [1.25x]  z2(16t) 698.3ms [2.03x]  z4(256t) 279.4ms [1.36x]  z6(3172t) 354.2ms [1.43x]  z8(4932t) 375.4ms [1.26x]
                    read 9.52 GiB in 5.1s (1923 MiB/s achieved)
  2048 MiB/s:       z0(1t) 746.3ms [1.18x]  z2(16t) 417.7ms [1.22x]  z4(256t) 259.0ms [1.26x]  z6(3172t) 320.8ms [1.29x]  z8(4932t) 356.6ms [1.20x]
                    read 7.96 GiB in 4.5s (1809 MiB/s achieved)
  1024 MiB/s:       z0(1t) 696.5ms [1.10x]  z2(16t) 355.7ms [1.04x]  z4(256t) 210.2ms [1.02x]  z6(3172t) 268.3ms [1.08x]  z8(4932t) 319.1ms [1.07x]
                    read 3.70 GiB in 3.7s (1024 MiB/s achieved)
  512 MiB/s:        z0(1t) 938.7ms [1.48x]  z2(16t) 331.7ms [0.97x]  z4(256t) 244.1ms [1.19x]  z6(3172t) 262.6ms [1.06x]  z8(4932t) 287.2ms [0.96x]
                    read 2.09 GiB in 4.2s (512 MiB/s achieved)
  128 MiB/s:        z0(1t) 642.8ms [1.01x]  z2(16t) 338.6ms [0.99x]  z4(256t) 209.9ms [1.02x]  z6(3172t) 250.8ms [1.01x]  z8(4932t) 290.6ms [0.97x]
                    read 0.44 GiB in 3.5s (128 MiB/s achieved)
  32 MiB/s:         z0(1t) 652.7ms [1.03x]  z2(16t) 364.3ms [1.06x]  z4(256t) 210.6ms [1.02x]  z6(3172t) 279.2ms [1.13x]  z8(4932t) 285.2ms [0.96x]
                    read 0.11 GiB in 3.6s (32 MiB/s achieved)
  quiet (after):    z0(1t) 628.5ms [0.99x]  z2(16t) 334.6ms [0.97x]  z4(256t) 232.6ms [1.13x]  z6(3172t) 241.0ms [0.97x]  z8(4932t) 286.2ms [0.96x]
```

### 200M base, 4 rounds of 2M — 7.84 GiB bundle against a 4 GiB page cache, **evicting**

Run under `systemd-run --user --scope -p MemoryMax=4G`, which charges page cache to the cgroup and
reclaims against it — the cheap way to reach the eviction regime without a bundle larger than the
machine. Two runs of the same configuration; the second adds the 2048 and 1024 MiB/s rates to
locate the knee.

```text
P3: base=200000000 rounds=4 batch=2000000
  bundle 7.84 GiB across 5 segments | page cache 4.00 GiB (cgroup memory.max) ⇒ EVICTING — the regime a fold actually creates
  quiet (before):   z0(1t) 101.6ms  z2(16t) 100.9ms  z4(256t) 115.4ms  z6(3172t) 78.5ms  z8(4932t) 113.0ms
  unthrottled:      z0(1t) 151.9ms [1.49x]  z2(16t) 124.7ms [1.24x]  z4(256t) 1.8s [15.67x]  z6(3172t) 98.7ms [1.26x]  z8(4932t) 128.9ms [1.14x]
                    read 8.79 GiB in 4.2s (2150 MiB/s achieved)
  512 MiB/s:        z0(1t) 102.7ms [1.01x]  z2(16t) 105.6ms [1.05x]  z4(256t) 121.1ms [1.05x]  z6(3172t) 83.6ms [1.06x]  z8(4932t) 123.4ms [1.09x]
                    read 0.60 GiB in 1.2s (512 MiB/s achieved)
  128 MiB/s:        z0(1t) 102.0ms [1.00x]  z2(16t) 104.3ms [1.03x]  z4(256t) 122.9ms [1.06x]  z6(3172t) 81.1ms [1.03x]  z8(4932t) 115.7ms [1.02x]
                    read 0.14 GiB in 1.1s (128 MiB/s achieved)
  32 MiB/s:         z0(1t) 100.4ms [0.99x]  z2(16t) 104.3ms [1.03x]  z4(256t) 121.8ms [1.06x]  z6(3172t) 81.0ms [1.03x]  z8(4932t) 112.5ms [1.00x]
                    read 0.04 GiB in 1.1s (33 MiB/s achieved)
  quiet (after):    z0(1t) 103.4ms [1.02x]  z2(16t) 104.8ms [1.04x]  z4(256t) 123.3ms [1.07x]  z6(3172t) 79.1ms [1.01x]  z8(4932t) 113.0ms [1.00x]
```

```text
P3: base=200000000 rounds=4 batch=2000000
  bundle 7.84 GiB across 5 segments | page cache 4.00 GiB (cgroup memory.max) ⇒ EVICTING — the regime a fold actually creates
  quiet (before):   z0(1t) 109.9ms  z2(16t) 113.5ms  z4(256t) 126.7ms  z6(3172t) 82.6ms  z8(4932t) 116.6ms
  unthrottled:      z0(1t) 150.4ms [1.37x]  z2(16t) 157.9ms [1.39x]  z4(256t) 139.8ms [1.10x]  z6(3172t) 107.7ms [1.30x]  z8(4932t) 143.0ms [1.23x]
                    read 3.88 GiB in 1.9s (2133 MiB/s achieved)
  2048 MiB/s:       z0(1t) 134.7ms [1.23x]  z2(16t) 152.1ms [1.34x]  z4(256t) 350.2ms [2.76x]  z6(3172t) 88.9ms [1.08x]  z8(4932t) 117.2ms [1.01x]
                    read 7.30 GiB in 3.7s (2048 MiB/s achieved)
  1024 MiB/s:       z0(1t) 110.0ms [1.00x]  z2(16t) 116.2ms [1.02x]  z4(256t) 128.2ms [1.01x]  z6(3172t) 86.7ms [1.05x]  z8(4932t) 129.7ms [1.11x]
                    read 1.20 GiB in 1.2s (1024 MiB/s achieved)
  512 MiB/s:        z0(1t) 112.0ms [1.02x]  z2(16t) 114.2ms [1.01x]  z4(256t) 129.7ms [1.02x]  z6(3172t) 88.2ms [1.07x]  z8(4932t) 120.9ms [1.04x]
                    read 0.59 GiB in 1.2s (512 MiB/s achieved)
  128 MiB/s:        z0(1t) 108.8ms [0.99x]  z2(16t) 110.6ms [0.97x]  z4(256t) 125.0ms [0.99x]  z6(3172t) 81.4ms [0.98x]  z8(4932t) 113.2ms [0.97x]
                    read 0.14 GiB in 1.1s (129 MiB/s achieved)
  32 MiB/s:         z0(1t) 108.1ms [0.98x]  z2(16t) 110.8ms [0.98x]  z4(256t) 125.4ms [0.99x]  z6(3172t) 81.0ms [0.98x]  z8(4932t) 112.8ms [0.97x]
                    read 0.04 GiB in 1.1s (32 MiB/s achieved)
  quiet (after):    z0(1t) 105.4ms [0.96x]  z2(16t) 109.6ms [0.97x]  z4(256t) 124.6ms [0.99x]  z6(3172t) 81.4ms [0.99x]  z8(4932t) 115.5ms [0.99x]
```

### 20M base, 4 rounds of 1M — 0.90 GiB bundle, fully page-cache resident

```text
P3: base=20000000 rounds=4 batch=1000000
  bundle 0.90 GiB across 5 segments — compare against this host's free RAM before quoting the ratio as a fold's
  quiet (before):   z0(1t) 12.8ms  z2(16t) 18.4ms  z4(256t) 27.4ms  z6(3172t) 45.6ms  z8(4932t) 75.9ms
  unthrottled:      z0(1t) 13.9ms [1.09x]  z2(16t) 16.1ms [0.87x]  z4(256t) 30.2ms [1.10x]  z6(3172t) 66.8ms [1.46x]  z8(4932t) 79.7ms [1.05x]
                    read 8.31 GiB in 455.7ms (18673 MiB/s achieved)
  512 MiB/s:        z0(1t) 14.6ms [1.14x]  z2(16t) 17.0ms [0.92x]  z4(256t) 32.5ms [1.19x]  z6(3172t) 48.9ms [1.07x]  z8(4932t) 77.2ms [1.02x]
                    read 0.21 GiB in 410.2ms (513 MiB/s achieved)
  128 MiB/s:        z0(1t) 12.6ms [0.98x]  z2(16t) 14.7ms [0.80x]  z4(256t) 27.6ms [1.01x]  z6(3172t) 46.9ms [1.03x]  z8(4932t) 77.6ms [1.02x]
                    read 0.05 GiB in 387.0ms (129 MiB/s achieved)
  32 MiB/s:         z0(1t) 12.8ms [1.00x]  z2(16t) 14.6ms [0.79x]  z4(256t) 27.6ms [1.01x]  z6(3172t) 47.6ms [1.04x]  z8(4932t) 85.7ms [1.13x]
                    read 0.01 GiB in 411.2ms (34 MiB/s achieved)
  quiet (after):    z0(1t) 14.0ms [1.09x]  z2(16t) 14.8ms [0.81x]  z4(256t) 27.5ms [1.01x]  z6(3172t) 44.1ms [0.97x]  z8(4932t) 75.7ms [1.00x]
```

### A discarded earlier run, kept because the methodology error is worth naming

Without the discarded warm-up sweep, the quiet baseline was the run's *first* sweep and came out
slower than every contended one — every ratio below 1.0:

```text
  quiet:            z0(1t) 18.5ms  z2(16t) 19.6ms  z4(256t) 40.3ms  z6(3172t) 76.1ms  z8(4936t) 134.1ms
  unthrottled:      z0(1t) 20.6ms [1.11x]  z2(16t) 16.2ms [0.83x]  z4(256t) 30.3ms [0.75x]  z6(3172t) 62.4ms [0.82x]  z8(4936t) 95.6ms [0.71x]
  512 MiB/s:        z0(1t) 15.1ms [0.81x]  z2(16t) 17.2ms [0.88x]  z4(256t) 31.9ms [0.79x]  z6(3172t) 53.7ms [0.70x]  z8(4936t) 88.3ms [0.66x]
  128 MiB/s:        z0(1t) 14.5ms [0.78x]  z2(16t) 17.1ms [0.87x]  z4(256t) 32.1ms [0.79x]  z6(3172t) 54.5ms [0.72x]  z8(4936t) 94.0ms [0.70x]
  32 MiB/s:         z0(1t) 13.9ms [0.75x]  z2(16t) 16.9ms [0.86x]  z4(256t) 32.1ms [0.80x]  z6(3172t) 53.4ms [0.66x]  z8(4936t) 87.9ms [0.66x]
```

Read as a measurement that reads as "streaming makes viewports faster". It is really "the first
sweep of a run is 1.4–1.8× the fifth", and it is why the probe now discards a warm-up sweep and
takes a second quiet baseline at the end as its noise floor.
