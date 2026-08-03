# The base dictionary does not need 7 GB: FST vs the in-memory map at 10⁸ terms

**Date:** 2026-08-03 · **Harness:** `dictprobe/` (standalone crate, not a workspace member)
**Raw:** `fullscale.log`, `fmix-shuf.log`, `surname-affine-shuf.log`, `trend-1e7-decimal.log`
**Machine:** WSL2, 47 GB, single-threaded runs · **Deps:** `fst 0.4`, `rustc-hash 2`, `memmap2 0.9`

Motivated by the descriptor-promotion design memo
(`docs/evidence/memos/2026-08-03-descriptor-promotion-design.md` §5): at the surnames config's
dictionary scale, `Dict`'s `FxHashMap<Box<[u8]>, TermId>` is the binding memory and restart cost,
not promotion's clone. n = 116,902,007 throughout — the surnames config at fold F=1, the largest
term cardinality any fixture here reaches (`probes/dataset.md` §"Fold F").

---

## Results

| namespace × ordinal model | FST file | map resident | open: FST / map | hit: FST / map |
|---|---|---|---|---|
| decimal, intern-order ordinals | 0.76 GB (6.52 B/key) | 7.09 GB | 0.04 ms / **40.0 s** | 1.0 µs / 348 ns |
| decimal, uncorrelated (fmix32) | 0.78 GB (6.70 B/key) | 7.09 GB | 0.03 ms / 49.7 s | 1.2 µs / 443 ns |
| surname, structured | **0.003 GB** (0.03 B/key) | 7.10 GB | 0.04 ms / 48.7 s | 0.6 µs / 422 ns |
| surname, uncorrelated (fmix32) | 0.79 GB (6.74 B/key) | 7.09 GB | 0.04 ms / 52.9 s | 1.3 µs / 430 ns |
| hex32 (adversarial random keys) | 4.06 GB (34.69 B/key) | 8.96 GB | 0.08 ms / 49.0 s | 1.8–2.2 µs / 522 ns |

- **The number to design against is ~6.7 B/key ≈ 0.8 GB — a 9× residency reduction** — and it is a
  *file*, not heap: resident set is the hot pages, eviction is the kernel's. With genuinely
  uncorrelated ordinals every namespace converges there, because value entropy (~4 B/key floor for
  27-bit ordinals) dominates once the automaton stops getting spurious output sharing.
- **The restart win may matter more than the memory win.** Building the map is an `Engine::open`
  cost paid on every restart — 40–53 s of not serving, at this scale, under a fail-closed posture.
  The FST mmaps in microseconds.
- **No key shape makes the FST worse than the map on memory.** 32-char random hex — zero shared
  structure — costs 34.7 B/key (slightly over raw key bytes, as theory predicts): 4.06 GB file
  against 8.96 GB heap.
- **The lookup tax is irrelevant at its consumers.** ~1.3 µs vs ~0.43 µs per probe. A token at the
  declared `max_terms_per_token` ceiling (100,000 descriptors) resolves in ~130 ms vs ~43 ms at
  authorise, against a measured 10.7 s row projection at 10⁹; ingest resolves per-item counts, far
  smaller.
- **Build cost is a one-off in `tessera build`:** sort + FST construction ~30 s at n = 1.17×10⁸
  (387 s for the adversarial hex namespace), noise inside the existing ~10-minute 1e9 pipeline.
- **Bytes/key is scale-stable, so extrapolation is legitimate:** hex32 34.58 → 34.69 and
  decimal-fmix 6.70 → 6.70 from 10⁷ to 1.17×10⁸; structured surname *improves* with scale
  (0.32 → 0.03) as the cross-product amortises.

## Negative result: an affine ordinal shuffle is NOT a valid pessimistic model

The first "shuffled" run used an odd-multiplier permutation mod 2^k and measured **0.27 B/key**
(`surname-affine-shuf.log`) — spuriously small. An affine map of a cross-product key language still
decomposes additively along FST paths, so the outputs share exactly the structure the model was
meant to remove. The murmur3 finalizer (non-linear bijection on u32) is the correct scatter:
**6.74 B/key** on the same keys. Do not quote the affine number; it is retained as the trap.

The structured-order floor (0.03 B/key) is equally real and equally not to be banked on: real
intern order is first-appearance order over the corpus, which lands between the two rows.

## Method

Three namespaces: `decimal` — decimal strings of 0..n, the descriptor bytes the probe bundles
actually intern (`tessera-build` joins source term ids as the access label, so `builtin:passthrough`
yields decimal-string descriptors); `surname` — the 404,104 real descriptors from
`data/pairs/surnames.terms.parquet` × replica suffix `@r<k>`, the realistic deployment shape;
`hex32` — 32 random hex chars, the incompressible bound. FST values are the intern-order ordinals,
exactly what `Dict::lookup` answers. Every run verifies 10⁶ random present keys resolve to their
ordinal and 10⁶ absent keys miss, on both structures — the checksums in the logs are equality
witnesses between FST and map.

Map residency is `/proc/self/statm` growth around construction; FST residency is growth around
mmap + two lookup passes. "Cold" pages are only page-cache-cold, not device-cold: the file was
written moments earlier, so device-cold first-touch latency is **not measured** here.

## Re-run

```bash
# the surname input (not committed): extract descriptors from the terms parquet
uv run --with pyarrow python -c "
import pyarrow.parquet as pq
t = pq.read_table('data/pairs/surnames.terms.parquet')
open('probes/2026-08-03-dict-fst/dictprobe/surnames.txt','w').write(
    '\n'.join(t.column('descriptor').to_pylist()))"

cd probes/2026-08-03-dict-fst/dictprobe && cargo build --release
PROBE_DIR=. ./target/release/dictprobe <decimal|surname|hex32> <n> [shuf]
```

## What this feeds

The promotion memo's §5: at large dictionaries the base representation, not promotion's
`load_extending` clone, is the binding constraint — and the exit is a derived, digest-gated,
mmap-able FST beside the canonical `terms-<k>.dict` record extents (which stay the format truth),
with a conformance obligation that FST lookup ≡ walking the records. Promotion composes unchanged:
flush extents and promoted-since-open terms stay in a small in-memory extension probed on base
miss.
