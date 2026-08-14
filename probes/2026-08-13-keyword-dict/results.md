# The decodable dictionary's real bytes, and the restart interval chosen by measurement

**Date:** 2026-08-13 · **Harness:** [`dictbytes/`](dictbytes/) · **Machine:** WSL2, AMD Ryzen 9
5900X, single-threaded · **Corpus:** the same three real arXiv columns
`2026-08-12-keyword-and-list-storage` measured, read as prefixes in snapshot order (= entity
order, `probes/dataset.md` §5 rule 1) from the Kaggle snapshot (v296): `id`, `submitter`, `doi`.

Commissioned by the keyword family's dictionary implementation (epic #85, T2). The parent
campaign's `front_coded()` charges one byte for the shared-prefix length and the suffix bytes —
**no suffix lengths and no restart offsets** — so what it reports cannot be decoded as stored, and
records-and-search §4.3 carries the gap as a model rather than a measurement: "~1–2 B/key more",
moving the headline "from 2.9× to ~2.2–2.3×" and the range across the three shapes to "~2.2–3.9×".
This campaign replaces that model with the shipped writer's output. Every figure below is
**measured** over `crates/tessera-filter/src/dict.rs` as it stands, not modelled.

The harness recomputes the parent's floor alongside, and reproduces it exactly — `id` 17.8 B/entity
flat against 6.1 DICT+C, `submitter` 22.3 against 5.7, `doi` 33.2 against 7.7 — so the two columns
of every table below differ only by the format, not by the corpus or the accounting.

## Results

### 1. The decodable format costs 2.0–2.9 B/key over the floor, and `doi` is outside the model

B per key at 2.4M; the trend is flat from 250k (read [`raw/dictbytes.txt`](raw/dictbytes.txt) for
all three scales — `id` moves 4.10 → 4.15 across a 9.6× range).

| column | distinct | probe floor | **K=8** | **K=16** | **K=32** | **K=64** | K=16 − floor |
|---|---|---|---|---|---|---|---|
| `id` | 2,400,000 | 2.11 | 5.19 | **4.15** | 3.63 | 3.37 | **+2.04** |
| `submitter` | 542,489 | 7.62 | 10.62 | **9.61** | 9.11 | 8.87 | **+2.00** |
| `doi` | 1,024,837 | 3.70 | 8.51 | **6.60** | 5.65 | 5.18 | **+2.90** |

**§4.3's modelled "~1–2 B/key more" holds for two columns and is exceeded by the third.** `id` and
`submitter` land at +2.0, at the top of the band; `doi` costs **+2.9**, outside it. The excess is
not slack in the format — it decomposes: one byte for the suffix-length varint, 0.5 B/key for the
`u64` restart offsets amortised over 16 keys, and the rest for the **elision the first key of every
block loses**. That last term scales with how much a column's keys share, which is why the
long, heavily-shared `doi` keys pay most: a key that would have elided ~20 bytes writes them out
once every 16 keys.

### 2. The layout still beats the flat column 2.2–3.6×, just under the modelled band

B per present entity at 2.4M. **DICT+C** is the dictionary at K=16 plus one `u32` ordinal per
present entity; **FLAT** is the shipped `utf8` column (bytes + `i64` offsets). Presence is common
to both and excluded from both, as in the parent campaign.

| column | FLAT | floor + C (the parent's figure) | **DICT+C, real** | floor × | **real ×** |
|---|---|---|---|---|---|
| `id` | 17.8 | 6.1 | **8.2** | 2.91 | **2.18** |
| `submitter` | 22.3 | 5.7 | **6.2** | 3.89 | **3.61** |
| `doi` | 33.2 | 7.7 | **10.6** | 4.31 | **3.13** |

**The measured range is 2.18–3.61×, against §4.3's modelled 2.2–3.9×** — marginally below the model
at both ends, and `id`'s 2.18 is just under the 2.2–2.3 the design predicted for it. The
direction the design rests on is unharmed: **the layout is smaller than the column it replaces on
every shape measured**, and the sign was the claim. The headline should be quoted as **2.2–3.6×
measured** rather than 2.2–3.9× modelled.

⊘ The parent campaign's caveat transfers unchanged and is the one that matters most: these are
arXiv-shaped keys. A prefix-free key set (UUIDs, hashes) front-codes to nearly its raw bytes, and
the layout then merely ties the flat column. No UUID-shaped column exists in this corpus.

### 3. K = 16 is kept, and what it costs is now on the record

The restart interval trades a restart offset and one un-elided key per block against `K/2` decodes
per lookup. Both halves, at 2.4M (`id`; ns per operation, single-threaded):

| K | B/key (`id`) | `resolve` | `key_of` | `walk` per key |
|---|---|---|---|---|
| 8 | 5.19 | 1122 | 87 | 11.7 |
| **16** | **4.15** | **1002** | **103** | **11.8** |
| 32 | 3.63 | 1040 | 135 | 11.0 |
| 64 | 3.37 | 1084 | 226 | 11.0 |

Doubling 16 → 32 saves **0.50–0.95 B/key** — 2% of the `submitter` layout, 6% of `id`'s, 9% of
`doi`'s — and costs **31% on `key_of`**, which is the narrow `contains` route's per-*candidate
entity* probe. K = 16 is kept because the bytes are a small fraction of a layout that already
beats the column it replaces several-fold, while the probe's constant is multiplied by the
candidate; and because K = 16 puts that probe at **0.10 µs, the bottom of §4.3's modelled
0.1–0.3 µs band** rather than its middle. A later reader with a different weighting has the table.

`resolve` is flat in K and an order of magnitude above `key_of` — ~1 µs against ~0.1 — because it
is 17 random accesses over a 10 MB structure while `key_of` is one. It is a per-*request* cost
against a per-entity scan measured in milliseconds, so it is not a term in any budget.

**The hit/miss columns in the raw output are not a timing-channel measurement** and must not be
read as one: the miss loop runs immediately after the hit loop over the same blocks, so it is
confounded by cache warmth. §4.3's channel argument does not rest on the lookup in any case — it
rests on the rule that **an unresolved needle still scans**, which makes the millisecond-scale work
that follows identical either way.

### 4. The broad `contains` route's cost model looks optimistic — flagged, not settled

§4.3 models the broad route at "order **2–10 s** single-threaded" at 10⁹ unique keys. **Decode
alone measures 11.0–18.8 ns/key** here, which multiplies to **11–19 s at 10⁹** — above that band
before the substring search each key must also receive, and before whatever penalty a structure
400× larger pays for leaving cache. This campaign does not settle it: 2.4M keys is a ~10 MB
dictionary that fits in L3, the extrapolation carries the same ⊘ as every other in this corpus,
and the walk measured here sums key lengths rather than searching them. **It is a flag for §11
item 4's harness**, which owes the real measurement, and a caution against quoting 2–10 s as
though it were measured.

## What this does not measure

- **Scale.** 2.4M, with bytes per key flat across a 9.6× range. 10⁹ is a further 400×, and the
  timing figures in particular would change character there — the dictionary stops fitting in
  cache.
- **The layouts other than DICT+C.** The parent campaign settled that comparison; nothing here
  re-runs IX+REC or the flat column's scan.
- **Prefix-poor key sets** — §2's caveat, and still no such column in this corpus.
- **Concurrency.** Single-threaded throughout, as the parent was.
