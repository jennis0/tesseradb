# P1 — raw output

**Host:** AMD Ryzen 9 5900X (12 cores), 47 GiB RAM, WSL2 6.18.33.2, `/dev/sdd` with 82 GiB free.
Release build, 2026-08-07.

**Harness:** `crates/tessera-engine/tests/scale.rs`,
`a_fold_over_a_multi_segment_corpus_at_two_sizes`, `#[ignore]`d.

```text
TESSERA_P1_SCALES=2500000,5000000,10000000 \
  cargo test -p tessera-engine --release --test scale -- \
  --ignored --nocapture a_fold_over_a_multi_segment_corpus_at_two_sizes
```

The analysis is `docs/evidence/memos/2026-08-07-compaction-fold-memory.md`. What is here is what the
runs printed.

**Every size runs in its own child process.** glibc's allocator does not return arenas to the
kernel, so a second fold in the same process peaks at whatever the first reached whether or not it
needed it — an artefact that reports perfect flatness for a fold with a corpus-sized `Vec` in it.

**Each half is measured against its own `clear_refs` reset.** `VmHWM` is the kernel's own
high-water mark and is exact; the anon/file split beside it is polled at 10 ms and is therefore a
lower bound. The pass staircase is `compact::PassCost`, sampled at pass boundaries by the fold
itself — it attributes, and it cannot see a peak *inside* a pass, which is what the `at t=` figure
on the `five passes` line is for.

---

## Run 1 — before the fixes (10⁷ only)

The run that found the defects. `RunCursor` still decoded every external-id run into the heap, and
`digest_of` still read each file whole.

```text
P1: base=10000000 rounds=4 batch=500000 deletions=10000
  fold: 6.3s | 3 segments → 1 | live 0.466 → 0.475 GiB | on disc before 0.538 GiB
    pass entry                30.2µs  rss   1.084 GiB  anon   0.990 GiB
    pass 1 row space            2.3s  rss   1.087 GiB  anon   0.993 GiB
    pass 2 postings          235.7ms  rss   1.092 GiB  anon   0.998 GiB
    pass 3 external ids         2.0s  rss   1.141 GiB  anon   1.047 GiB
    pass 5 digests + fsync      1.5s  rss   1.141 GiB  anon   1.047 GiB
  five passes:      6.1s  peak Δ   0.445 GiB at t=4.5s   (spec §3's budget covers this half)
  publication:   191.9ms  peak Δ   0.046 GiB   (spec §4; no memory budget states this half)
  peak RSS 1.528 GiB over a 1.084 GiB baseline ⇒ Δ 0.445 GiB; anon Δ 0.255 GiB, file Δ 0.366 GiB
```

`t=4.5s` sits inside pass 3, which ends at 4.54 s. The staircase shows the resident set *at the
pass boundary* climbing only 57 MB across the whole fold — the 445 MB peak is entirely intra-pass
and comes back down before pass 3 returns, which is why boundary sampling alone could not have found
this and the sampler's timestamp could.

## Run 2 — after the streaming digest, before the mapped run cursor

```text
  five passes:      5.1s  peak Δ   0.444 GiB at t=4.6s
  publication:   212.5ms  peak Δ   0.046 GiB
  peak RSS 1.512 GiB over a 1.068 GiB baseline ⇒ Δ 0.444 GiB; anon Δ 0.255 GiB, file Δ 0.370 GiB
    pass 5 digests + fsync   390.5ms   (was 1.5s)
```

Pass 5 got ~4× faster and the peak did not move: the whole-file read was real and was not the peak.

## Run 3 — after both fixes, three sizes

```text
P1: 3 size(s) [2500000, 5000000, 10000000], one child process each

P1: base=2500000 rounds=4 batch=125000 deletions=10000
  fold: 1.2s | 3 segments → 1 | live 0.116 → 0.118 GiB | on disc before 0.134 GiB
    pass entry                56.2µs  rss   0.427 GiB  anon   0.396 GiB
    pass 1 row space         577.6ms  rss   0.428 GiB  anon   0.397 GiB
    pass 2 postings           66.0ms  rss   0.433 GiB  anon   0.402 GiB
    pass 3 external ids      292.2ms  rss   0.442 GiB  anon   0.411 GiB
    pass 5 digests + fsync   120.7ms  rss   0.442 GiB  anon   0.411 GiB
  five passes:      1.1s  peak Δ   0.108 GiB at t=923.7ms
  publication:   173.3ms  peak Δ   0.011 GiB
  peak RSS 0.536 GiB over a 0.427 GiB baseline ⇒ Δ 0.108 GiB; anon Δ 0.014 GiB, file Δ 0.092 GiB

P1: base=5000000 rounds=4 batch=250000 deletions=10000
  fold: 2.1s | 3 segments → 1 | live 0.233 → 0.237 GiB | on disc before 0.269 GiB
    pass entry                29.6µs  rss   0.620 GiB  anon   0.568 GiB
    pass 1 row space            1.3s  rss   0.622 GiB  anon   0.570 GiB
    pass 2 postings          121.6ms  rss   0.626 GiB  anon   0.574 GiB
    pass 3 external ids         1.1s  rss   0.647 GiB  anon   0.595 GiB
    pass 5 digests + fsync   344.0ms  rss   0.647 GiB  anon   0.595 GiB
  five passes:      2.0s  peak Δ   0.216 GiB at t=1.7s
  publication:   101.7ms  peak Δ   0.023 GiB
  peak RSS 0.840 GiB over a 0.624 GiB baseline ⇒ Δ 0.216 GiB; anon Δ 0.027 GiB, file Δ 0.182 GiB

P1: base=10000000 rounds=4 batch=500000 deletions=10000
  fold: 4.3s | 3 segments → 1 | live 0.466 → 0.475 GiB | on disc before 0.538 GiB
  five passes:      4.1s  peak Δ   0.463 GiB at t=3.6s
  publication:   198.4ms  peak Δ   0.046 GiB
  peak RSS 1.483 GiB over a 1.019 GiB baseline ⇒ Δ 0.463 GiB; anon Δ 0.083 GiB, file Δ 0.380 GiB

          base   input GiB    peak GiB   passes Δ  publish Δ   peak/in  B/entity     passes    publish
       2500000       0.116       0.108      0.108      0.011     0.932      37.2      1.1s   173.0ms
       5000000       0.233       0.216      0.216      0.023     0.929      37.2      2.0s   101.0ms
      10000000       0.466       0.463      0.463      0.046     0.993      39.8      4.1s   198.0ms

  ⇒ 39.8 B/entity resident at base=10000000, of which 7.1 B is anonymous. Projected to 10⁹
    entities: 39.8 GB resident, 7.1 GB anonymous.
```

---

## What the corpus is, and where it does not stand in for a deployment

`build_fixture_n` plus four published ingest rounds of `base/20` and one more whose 10,000 entity
ids are deleted before the fold, so the fold has three segments to consume and a non-empty `D₀`.

**Its dictionary is two terms.** Every measured figure is therefore free of `PostingsSpool`'s
offsets buffer, which `compaction.md` §3 models at ~0.94 GB at 1.17×10⁸ terms and which is the one
budgeted term this corpus cannot exercise. The projections above are the entity-space half only, and
the memo adds the dictionary term separately rather than pretending it was measured.

**Its external ids are ~11 bytes** (`p3-i124999`). A deployment with longer keys moves pass 3's
input bytes and therefore the file-backed half; it does not move the anonymous half, which is what
the budget is about.
