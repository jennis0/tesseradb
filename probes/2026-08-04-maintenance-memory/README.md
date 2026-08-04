# A merge's peak memory, measured

**Date:** 2026-08-04 · **Tool:** `crates/tessera-store/examples/maintenance_memory.rs` · **Raw:**
`runs.txt`

The named gap in `docs/design/write-path.md` §12: *"no maintenance event's peak memory has ever
been measured"*. Merge execute was carried as **modelled** at ≈5–7× its inputs' on-disk bytes,
and operators size boxes from that number, because `max_merged_segment_bytes` bounds the
*selection-time file bytes* rather than the decoded resident set.

Re-run:

```
cargo run --release --example maintenance_memory -p tessera-store -- [--rows N] [--segments K]
```

## Result: **4.4–4.9×**, and stable across shape

| inputs | shape | `VmHWM` | multiple |
|---|---|---|---|
| 77.9 MB | 4 × 500 k rows | 377.8 MB | **4.9×** |
| 314.9 MB | 4 × 2 M rows | 1 378.9 MB | **4.4×** |
| 314.9 MB | 8 × 1 M rows | 1 397.8 MB | **4.4×** |

The model was **conservative in the safe direction**, which is the right direction for a figure a
memory limit is set from, and it holds its shape: the same input bytes cost the same peak whether
they arrive as four segments or eight, so the multiplier is a function of the bytes rather than of
the segment count. Restating write-path §7's sizing note with the measured figure: a **256 MiB cap
models to a ~1.1–1.3 GB pool transient**, not the ~1.3–1.8 GB the modelled range gave.

Where it goes: every input is decoded to `TilerItem`s at once (the code, the residual, the
identity, the scalars), and `sort_batch` doubles that at the sort. The inputs' own mapped pages
are resident throughout and count too — deliberately, since a container limit counts them.

## Method, and what it does not cover

`VmHWM` from `/proc/self/status` — the kernel's own resident-set high-water, which is what a
cgroup limit is compared against. **Each stage runs in a fresh child process**: the mark is
monotone within a process, so running two stages in one would attribute the first's peak to the
second. The baseline reported beside each figure is the mark *before* the merge, i.e. what writing
the inputs already cost.

Three things this does not measure, stated rather than implied:

- **Concurrency.** A flush, a merge and a coalesce can run on the pool at once and their peaks
  add. Nothing here bounds the sum.
- **Tier coalescence**, still modelled at ≈2–3× the pairs' bytes. Its inputs are postings rather
  than rows, so this probe's shape does not transfer; it needs its own.
- **Allocator retention** as distinct from demand. `VmHWM` is what the process ever held, which is
  the operator-facing quantity, not the smallest it could have run in.
