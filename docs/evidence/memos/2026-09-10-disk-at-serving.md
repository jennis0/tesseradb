# What serving costs on disk

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified the fragment cache's `.frag` tier is explicitly outside `set_memory_bound` and `superseded_entries` has one caller, the fold; `pairs.parquet` has no serve-path reader; and `contracts.md`'s "the postings are emitted and not read at serving" is contradicted by the readers in `filter.rs`. **Every other claim here is unverified** — re-check a
figure or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's
starting point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which
these supersede in the places they contradict it.

The other fact-finds in this set are the sibling `2026-09-10-disk-*.md` memos in this
directory.

**Date** 2026-09-10. Read-only fact-finding against `main` at `0c0b60b3`. Every figure is marked
**measured** (taken here, or quoted from a probe that took it), **read-from-code**, **modelled**
or **assumed**.

## The result

A serving process costs **the bundle, plus about 1.5× the bundle again if it ever folds, plus an
unbounded fragment-cache directory**. The bundle itself is 68.5 to 771 B/item depending on schema.
Of it, **the viewport path reads 21–50 B/item on a geographic corpus and 26–28 B/item on a text
one**; the rest is read by a filter, by a layer, by a drill-down, or by nothing at all.

Four numbers, all measured here:

| | |
|---|---|
| **What every request path could ever read** | 68.5 B/item (GBIF at 125.8×10⁶), 82.2 (the same schema with minted external ids), 187.9 (TreeOfLife-1M), 693.2 (PaperSeek at 102×10⁶) |
| **What a viewport with no filter and no layer reads** | **31.2%** of the GBIF bundle, **3.7%** of PaperSeek's |
| **What no request path ever reads** | `terms/pairs.parquet` and `reports/`: 0.008–0.12 B/item. Already removable at build with `--no-oracle-pairs` |
| **What a fold needs free beside the bundle** | **150%** of live bytes (`compact.rs:640`), so peak disc is ~2.5× the bundle |

The single largest avoidable-without-latency item is corpus-dependent and it is never the viewport
path. On a text corpus it is the record blob (434 B/item, 62.6% of PaperSeek) which only
`/v1/items` and a phrase verify read. On a schema built with `--mint-external-ids` it is the
external-ID sidecar (20.25 B/item, 24.6% of the GBIF-64p bundle), which the open does not even
verify.

---

## 1. What a served bundle holds at rest

### Method

`st_blocks × 512` over every file under the bundle root, classified by path, divided by
`MANIFEST.json`'s `entity_id_high_water`. Script:
`/tmp/.../scratchpad/classify.py`. The 125.8×10⁶ GBIF row is reconstructed from
`probes/2026-09-10-build-disk/200m-after.files.tsv` (its `max_blocks` column, excluding
`.build-tmp/` and the `.spool`/`.spill`/`.scratch` files the build unlinks); it reproduces the
probe's 68.5 B/item to the second decimal, so the classification agrees with the probe's own
total.

**Caveat on which bundles are current.** `data/` holds seven built bundles. Only two are at the
reader's `BUNDLE_FORMAT = 8` (`crates/tessera-types/src/lib.rs:135`): `gbif-64p` and
`treeoflife-1m`, plus the probe's GBIF listing. `geonames/bundle-final` is format 3 and
`medcpt-1m`, `medcpt-10m-abs` and `rung4-run` are format 5, so today's `open_bundle` would refuse
all four (`read.rs:459`). Their *shapes* are still the right evidence about what a wide-schema,
text-heavy or DAG-layer corpus writes, and they are marked below.

### Measured, six corpora

| bundle | fmt | items | total | **B/item** |
|---|---|---|---|---|
| GBIF, probe listing | 8 | 125,789,091 | 8.618 GB | **68.51** |
| `gbif-64p` (same schema, `--mint-external-ids`) | 8 | 25,846,007 | 2.124 GB | **82.18** |
| `treeoflife-1m` | 8 | 1,000,000 | 0.188 GB | **187.92** |
| `geonames/bundle-final` ⊘ fmt 3 | 3 | 13,463,857 | 1.398 GB | **103.84** |
| `medcpt-1m/bundle-probe` ⊘ fmt 5 | 5 | 1,000,000 | 0.333 GB | **332.80** |
| `medcpt-10m-abs/bundle-auto` ⊘ fmt 5 | 5 | 10,000,000 | 7.706 GB | **770.57** |
| `rung4-run` (PaperSeek) ⊘ fmt 5 | 5 | 102,117,343 | 70.783 GB | **693.16** |

### The 68.5 B/item, decomposed (GBIF, 125,789,091 items, measured)

One `geo` view, four declared attributes, a three-level tiered taxonomy, no external ids.

| file kind | n | MB | **B/item** | % |
|---|---:|---:|---:|---:|
| `views/geo/segments/seg-0/columns.arrow` | 1 | 1682.4 | **13.375** | 19.5 |
| `row-column/*.tslb` | 3 | 1257.9 | **10.000** | 14.6 |
| `attrs/record/blocks.bin` | 1 | 1031.7 | **8.202** | 12.0 |
| `entities/terms/{offsets,terms,hasrow}` | 3 | 1006.4 | **8.000** | 11.7 |
| `members/*.tsmb` | 3 | 843.8 | **6.708** | 9.8 |
| `attrs/*/values.arrow` | 2 | 747.2 | **5.940** | 8.7 |
| `attrs/record/directory.arrow` | 1 | 519.6 | **4.131** | 6.0 |
| `views/geo/permutation.bin` | 1 | 503.3 | **4.001** | 5.8 |
| `views/geo/segments/seg-0/morton.u32` | 1 | 503.2 | **4.000** | 5.8 |
| `views/geo/row-entity.u32` | 1 | 503.2 | **4.000** | 5.8 |
| `attrs/*/presence.roaring` | 2 | 13.6 | 0.108 | 0.2 |
| `containment/*.tscp` | 3 | 2.4 | 0.019 | 0.0 |
| `attrs/specieskey/dict.bin` | 1 | 2.3 | 0.018 | 0.0 |
| `terms/pairs.parquet` | 1 | 1.1 | 0.008 | 0.0 |
| `terms/postings.arrow`, `record/hasrow.roaring`, `reports/`, `MANIFEST`, `CURRENT`, `SEGMENTS-0.json`, `dictionary/` | 7 | 0.11 | <0.001 | 0.0 |

Ten files carry 99.8% of it, and every one is a fixed number of bytes per item: 4 B/item each for
the permutation, the Morton column, the row→entity table and a `u32` render column; 12 B/item for
`tessera_id` and `residual` in `columns.arrow`; 8 B/item for the postings transpose.

### The same decomposition on a wide schema (TreeOfLife-1M, format 8, measured)

Two views, six attribute columns (two text-indexed, three keyword-dictionaried), a nine-file
tiered taxonomy, external ids minted.

| file kind | B/item | % |
|---|---:|---:|
| `views/*/segments/*/columns.arrow` (2 views, 12 render columns) | **52.158** | 27.8 |
| `attrs/*/dict.bin` (`uuid` alone is 34.5 MB) | **35.275** | 18.8 |
| `members/*.tsmb` | 18.653 | 9.9 |
| `entities/external-ids-0.arrow` | 16.253 | 8.6 |
| `attrs/*/values.arrow` | 11.510 | 6.1 |
| `views/*/permutation.bin` | 8.397 | 4.5 |
| `entities/terms/*` | 8.008 | 4.3 |
| `views/*/row-entity.u32` | 7.045 | 3.7 |
| `views/*/segments/*/morton.u32` | 7.045 | 3.7 |
| `MANIFEST.json` (46,578 vocabulary values, inline) | **5.063** | 2.7 |
| `row-column/*.tslb` | 4.571 | 2.4 |
| `entities/ext-locator.u32` | 4.002 | 2.1 |
| `attrs/*/postings.arrow` | 3.219 | 1.7 |
| `attrs/record/{blocks,directory,hasrow}` | 6.075 | 3.2 |
| everything else | 0.63 | 0.3 |

### And on a prose corpus (PaperSeek, 102,117,343 items, ⊘ format 5, measured)

| file kind | B/item | % |
|---|---:|---:|
| `attrs/record/blocks.bin` | **434.176** | 62.6 |
| `attrs/{abstract,title}/postings.arrow` | **192.973** | 27.8 |
| `views/*/segments/*/columns.arrow` | 17.750 | 2.6 |
| `attrs/*/dict.bin` | 8.813 | 1.3 |
| `attrs/*/values.arrow` | 8.120 | 1.2 |
| `entities/terms/*` | 8.000 | 1.2 |
| `members/*.tsmb` | 4.765 | 0.7 |
| `attrs/record/directory.arrow` | 4.284 | 0.6 |
| `permutation.bin` / `morton.u32` / `row-entity.u32` | 12.002 | 1.7 |
| `row-column/*.tslb` | 2.000 | 0.3 |
| everything else | 0.28 | 0.0 |

`docs/ingest-campaign.md:867` records the same shape independently: "88% of the bundle is prose —
the record blob and the abstract index are 62.4 GB of 70.8 GB".

### Who writes each, and who reads it at serve (read-from-code)

| file | written by | read at serve by | size |
|---|---|---|---|
| `CURRENT` | every publication | `open_bundle` (`read.rs:361`) | 113 B |
| `MANIFEST.json` | build, fold | open: digest-checked whole, `serde_json` parsed, vocabularies held resident | grows with vocabulary values, not items |
| `dictionary/terms-*.dict` | build, flush, coalesce | `Dict::load` (`authz/dict.rs:272`) reads whole into an `FxHashMap` at open | per distinct access descriptor |
| `SEGMENTS-<n>.json` | flush, merge, deny publication | `load_verifying_segments_manifest` at open, highest first | per publication; old ones are never deleted |
| `terms/postings.arrow` | build, fold | `PostingsReader::open(.., true)` mmap at open (`session.rs:1235`); read per authorise | 0.0003 B/item on GBIF |
| `…/segments/*/delta.arrow` | flush | `DeltaTier::open` at open, unioned per authorise | per flush |
| `terms/pairs.parquet` | build, fold pass 2 | **nothing** | 0.008 B/item |
| `entities/terms/*` | build, flush | `EntityTermsStack::open` at open (`filter.rs:2012`), `terms_of` on `/v1/items` and the ingest join rule | 8.00 B/item |
| `entities/external-ids-*.arrow`, `ext-locator.u32` | build (`--mint-external-ids`), flush | nothing at open; `/v1/items`, `/control/ingest` dedup, `/control/changes` | 20.25 B/item where minted |
| `attrs/<col>/values.arrow` + `presence.roaring` | build, flush | mapped at open; filter scan, `/v1/categories` | 5.9–14.1 B/item |
| `attrs/<col>/postings.arrow` | build, fold | mapped at open; `eq`/`in` on a public category, the `derived` gate, `text_match` | 3.2 B/item (GBIF) to 193 (PaperSeek) |
| `attrs/<col>/dict.bin` | build, flush | mapped; needle resolution for `match`/`contains` | 0.018–35.3 B/item |
| `attrs/record/{blocks,directory,hasrow}` | build, flush, coalesce, fold | mapped at open; `/v1/items`, phrase verify (`filter.rs:3077`), artifact content | 12.3 B/item (GBIF) to 438 (PaperSeek) |
| `views/*/permutation.bin` | build, fold | `Permutation::load` at open, `validate_rows` over the whole bound; every entity→row crossing | 4.00 B/item per view |
| `views/*/row-entity.u32` | build, fold | mapped; **filtered viewports only** (`row_entity.rs:5`) | 4.00 B/item per view |
| `views/*/segments/*/morton.u32` | build, flush, merge, fold | mapped; `tile_ranges` binary search | 4.00 B/item per view |
| `views/*/segments/*/columns.arrow` | build, flush, merge, fold | mapped; the gather | 12 B + render widths |
| `members/*.tsmb` | layer publication, fold | `MembershipPack::open` mmap; read whole at open by `warm_artifact_projections` | 4.8–63 B/item |
| `row-column/*.tslb` | fold | `LabelColumnPack::open` mmap; transposed at open | 2–96 B/item |
| `tile-index/*.tsti` | fold | mmap; artifact candidacy | ≤0.3 B/item |
| `containment/*.tscp` | fold | mmap; the I3 containment test | ≤0.5 B/item |
| `reports/*.json` | build, fold | **nothing** | ~20 KB |

---

## 2. What the server never reads

### Never read by any request path, in any deployment

| file | evidence | cost |
|---|---|---|
| `terms/pairs.parquet` | no reader in `tessera-{server,engine,store,authz,filter}`; `pairs.rs:13` says "optional to *read*"; contracts §2.4 says "read at build cadence and by the oracle only"; the CLI already carries `--no-oracle-pairs` | **0.008–0.12 B/item** (1.06 MB at 125.8×10⁶, 0.86 MB at 102×10⁶) |
| `reports/disclosure.json`, `reports/containment.json` | written by `tessera-build/src/disclosure.rs` and `lib.rs:2436`; no reader outside the build; not in any `files` map, so not even digested at open | ~20 KB, flat |

That is the whole of the "never" list. Everything else in a bundle has a serve-time reader on
some request kind.

### Never read for a deployment that does not serve that request kind

Buckets, measured per corpus. A = a viewport with no filter and no layer, B = layers
(`/v1/viewport?layers`, `/v1/artifacts`, browse), C = filters and `/v1/categories`, D = the
`/v1/items` drill-down, E = never.

| corpus | A viewport | B layers | C filter | D drill-down | E never |
|---|---:|---:|---:|---:|---:|
| GBIF 125.8×10⁶ | 21.38 (31.2%) | 16.73 (24.4%) | 10.07 (14.7%) | **20.33 (29.7%)** | 0.009 |
| `gbif-64p` | 21.38 (26.0%) | 11.04 (13.4%) | 10.02 (12.2%) | **39.73 (48.3%)** | 0.010 |
| `treeoflife-1m` | 72.71 (38.7%) | 23.78 (12.7%) | 57.05 (30.4%) | 34.34 (18.3%) | 0.045 |
| `geonames` ⊘ | 50.36 (48.5%) | 8.59 (8.3%) | 32.22 (31.0%) | 12.67 (12.2%) | 0.011 |
| `medcpt-10m-abs` ⊘ | 28.40 (3.7%) | 150.84 (19.6%) | 193.00 (25.0%) | **398.27 (51.7%)** | 0.053 |
| PaperSeek 102×10⁶ ⊘ | 25.75 (3.7%) | 6.77 (1.0%) | 213.98 (30.9%) | **446.46 (64.4%)** | 0.009 |

Named individually, the four largest "present in every bundle, read by one request kind" items:

1. **The record blob** (`attrs/record/`) — 12.3 B/item on GBIF, **438 B/item and 62.6% of the
   bundle** on PaperSeek. Its only serve readers are `/v1/items` (`viewport.rs:2149`), artifact
   content (`viewport.rs:5948`), and the phrase verify (`filter.rs:3077`, which reads one block
   per surviving entity). Nothing on the viewport, count, or category path touches it.
   Read-from-code.

2. **The external-ID sidecar** — `entities/external-ids-*.arrow` + `ext-locator.u32`, **20.25
   B/item and 24.6% of `gbif-64p`**. Read only by `/v1/items`, `/control/ingest`'s duplicate
   check and `/control/changes` (`sidecar.rs:20`). It is already exempted from the open's digest
   sweep (`read.rs:1172`, contracts §0.3 deviation 9), so a serving process that answers no
   drill-down and no ingest never opens the file at all. Measured: it appears only because
   `gbif-64p` was built with `--mint-external-ids` (a benchmark fixture flag, off by default); the
   125.8×10⁶ GBIF bundle carries none and that is the whole of the 82.18 vs 68.51 gap.

3. **`entities/terms/`** — a flat **8.00 B/item on every corpus** (it is `offsets.u32` +
   `terms.u32`, one `u32` each per entity and per term). Read by `/v1/items`'s `labels` array
   (`viewport.rs:1865`) and by the ingest join rule (`views.md` §4). Contracts §2.4 marks it
   **"required, not optional"** and the open refuses without it (`filter.rs:2012` passes
   `Some(base)` unconditionally), so a deployment serving neither drill-down nor ingest carries
   8 B/item it will not read and cannot omit.

4. **`views/*/row-entity.u32`** — **4.00 B/item per view**. `row_entity.rs:5`: "for the one caller
   that needs it: a **filtered viewport**". A deployment that serves no filter never reads it.

### What the open reads even when serving does not

`open_bundle` hashes every file both `files` maps name, in 1 MiB chunks, in a rayon sweep
(`read.rs:1261`, `1184`). Measured bytes hashed at open:

| bundle | named | sidecar-exempt | **hashed at open** |
|---|---:|---:|---:|
| `gbif-64p` | 2123.8 MB | 523.4 MB | 1600.4 MB (61.92 B/item) |
| `treeoflife-1m` | 182.7 MB | 20.3 MB | 162.4 MB (162.43 B/item) |
| `geonames` ⊘ | 1341.8 MB | 0 | 1341.8 MB (99.66 B/item) |
| `medcpt-1m` ⊘ | 332.7 MB | 0 | 332.7 MB (332.69 B/item) |

So a serving replica reads and SHA-256s essentially the whole bundle at every start, and the only
kind exempt is the sidecar. Modelled at rung 6's 244 GB bundle that is 244 GB read and hashed per
restart. The bytes go through a fixed 1 MB buffer per file and are not held
(`probes/2026-09-02-serve-under-memory-cap/README.md:124` confirms by `smaps`), so the cost is
time and page-cache churn, not residency.

---

## 3. What the server writes or holds beyond the bundle

Two directories, named in `tessera.toml`'s `[bundle]`: `cache` and `wal`.

### 3.1 The fragment cache — on disk, and unbounded between folds

`crates/tessera-authz/src/fragment.rs`. One `<hex key>.frag` (the frozen CRoaring bytes of a
principal's authorised entity set) and one 48-byte `.meta` per canonical key. Measured on the
repo's own runtime directories: `data/ladder/treeoflife/.tessera/cache` holds 503 pairs at
4.5 MB; `overture` holds 845 at 7.1 MB.

- **Key** is `SHA-256(bundle_identity ‖ auth_plugin_hash ‖ watermark ‖ sorted term ids)`
  (`fragment.rs:137`). Read-from-code.
- **Size per entry** is the frozen bitmap: ~2 B × visible cardinality, with a dense bound. The
  measured dense figure at 10⁹ over a ≥25% grant is **125.12 MB**
  (`config.rs:1928`, `MEASURED_PROJECTION_BYTES_AT_1E9 = 125_120_000`).
- **Bound:** `set_memory_bound` bounds the **in-memory** tier only. `fragment.rs:660` states it:
  "The digest-verified `.frag` sidecar tier is untouched by it". `evict` deliberately leaves the
  pair on disk (`fragment.rs:683`).
- **The only removal route is a compaction fold.** `superseded_entries` + `sweep` are called from
  exactly one place, `write.rs:16748`/`16766`, in the fold's publication. `fragment.rs:740` records
  the owner ruling: "**No format version and no orphan sweep**".
- **Growth:** one pair per (grant set × watermark) that authorises. The watermark advances at each
  flush publication that raises it (`manifest.rs:1761`), so an ingesting deployment mints a fresh
  entry for the same credential after every flush. Under `caching.md` §3's stated operating point
  (100–1000 users, grant sets "assumed near-unique", 90 s flush tick), this directory grows without
  a ceiling until a fold runs. Read-from-code; **not measured at scale**.

### 3.2 The suggestion index — on disk, bounded, rebuilt every open

`crates/tessera-engine/src/suggest.rs`, under `<cache>/suggest/`. Six mapped files per vocabulary
that a declared category column names. `Engine::open` does `remove_dir_all` on the directory and
rebuilds (`session.rs:1647–1649`), so it never accumulates across restarts. Measured:
`data/ladder/treeoflife/.tessera/cache/suggest` is 32.1 MB over eleven vocabularies (species 18.0 MB,
genus 10.4 MB). It scales with **distinct vocabulary values**, not with items.

### 3.3 The WAL — a sequence, reclaimed at flush, with an unenforced ceiling

`crates/tessera-lifecycle/src/wal.rs`. `<dir>/wal-<n>.log` plus an 8-byte `<n>.sync` sidecar per
member. A rotation seals the active member and deletes whatever lies wholly behind a flush's
`wal_pos`, so steady-state size is roughly one flush interval of ingest plus the standing overlay
snapshot at each member's head.

`ingest.wal_hard_limit_bytes` defaults to 8 GiB, and `config.rs:1766` says plainly what it is:
"⊘ **The name reads as a runtime ceiling and is not one**: `Wal` exposes no length accessor, so
nothing can compare the live WAL against this number… past that point the WAL grows until the
filesystem refuses, at which point `WalError::Poisoned` makes the handle dead." So the bound is a
startup relation only. Read-from-code.

### 3.4 The bundle itself grows while serving

- **Every flush appends** a segment (`morton.u32`, `columns.arrow`, optionally `delta.arrow`,
  `external-ids.arrow`, `ext-locator.u32`, `terms-0.dict`), one `entities/terms` extent, one
  attribute extent per filterable column, one record-blob extent triple, and a new
  `SEGMENTS-<n+1>.json`. **The old `SEGMENTS-<n>.json` files are not deleted.** Measured on the
  repo's `data/scratch/`: the same 1M-item corpus is 187.89 MB after a build and **200.06 MB after
  13 flushes** (`multiseg-1m`), 369 files against 63, with 15 `SEGMENTS-*.json` at 0.67 MB.
- **Every deny publication** writes a `SEGMENTS-<n+1>.json` carrying the whole deny set.
- **A fold doubles the bundle for its flight.** `reclaim.rs:13`: "peak disc is old prefix + new
  prefix — roughly 2× live bytes". The pre-flight demands **150% of live bytes free**
  (`compact.rs:640`, `FOLD_DISC_PERCENT`), so the deployment sizing figure is **~2.5× the bundle**.
  That percentage is marked **assumed** at the constant: the 50% margin covers the flushes, WAL and
  fragment cache that land beside the new prefix during a flight of minutes to hours.

### 3.5 What is *not* on disk

- Every server cache in `caching.md` §5 that exists (S1 mask fragment, S2 row projection, S2b
  masked-count histogram) is a `SingleFlightCache` in memory with a byte bound. Only S1's
  spill-through is on disk, and that is §3.1.
- The **filter clause cache** (`filter-result-cache.md`) is not built. Its status line says so.
- **Delta serving** (`delta-serving.md`) keeps its replica on the client. Nothing server-side.
- The **artifact fetch protocol** and `artifact-serving-at-scale.md` name no disk state; the
  structures they size are resident (§4).
- No temp files. Every `tempfile`/`TempDir` in `tessera-engine`, `tessera-server` and
  `tessera-lifecycle` is inside a `#[cfg(test)]` module.

---

## 4. Mapped versus resident

### Read whole at open, into the heap

| structure | source | size grows with |
|---|---|---|
| the access dictionary | `Dict::load` `std::fs::read` + `FxHashMap<Box<[u8]>, TermId>` (`authz/dict.rs:277`) | distinct access descriptors |
| `MANIFEST.json`, parsed | `serde_json::from_slice` (`read.rs:452`) | **vocabulary values**. Measured: geonames' manifest is 56.0 MB of which 34.1 MB is `vocabularies` (465,765 values); treeoflife-1m's is 5.06 MB for 46,578 |
| every `SEGMENTS-<n>.json` walked | `load_verifying_segments_manifest` | deny-set size, extent counts |
| `attrs/record/hasrow.roaring` | `std::fs::read` + `Bitmap::try_deserialize` (`filter/record.rs:503`) | run-optimised; negligible (8 KB at 25.8×10⁶) |
| `entities/terms/hasrow.roaring` | same (`entity_terms.rs:274`) | same |

### Mapped, demand-paged, never read whole

`columns.arrow`, `morton.u32`, `permutation.bin`, `row-entity.u32`, `values.arrow`,
`presence.roaring`, both kinds of `postings.arrow`, `dict.bin`, `blocks.bin`, `directory.arrow`,
`*.tsmb`, `*.tslb`, `*.tsti`, `*.tscp`, `delta.arrow`, and the external-ID sidecar. The filter
side is opened with `Access::Mapped` explicitly and the reason is in the call site
(`session.rs:1621`): "the engine opens every declared column at once and holds them for the process
lifetime, so the alternative is tens of GB of residency at 10⁹ paid before any filter arrives".

This is **measured, not just claimed**. `probes/2026-09-02-serve-under-memory-cap/README.md:128`:
"`smaps` confirms `attrs/title/postings.arrow` (824 MB) and `attrs/mesh_major/postings.arrow`
(365 MB) are resident as **file**, not anon, when touched." And the whole-system version, measured
at rung 4: a **70.78 GB bundle served under a 24 GiB cgroup cap** with `oom_kill 0`, 110,266
reclaim-at-max events, and every count identical to the uncapped run
(`docs/ingest-campaign.md:878`).

### Derived at open and held resident — this is the term that does not page out

`Engine::warm_artifact_projections` (`viewport.rs:4875`) builds, before the listener binds, every
`(view, layer, level)`'s artifact-major row form out of the mapped `.tsmb`/`.tslb`. Measured
anonymous RSS at rest, after `/readyz` and before any request:

| rung | items | artifacts | anon at open |
|---|---:|---:|---:|
| PaperSeek at a 10⁷ prefix | 10,000,000 | two layers, four levels | **166 MB** |
| MedCPT (`mesh/descriptors`, 1.66×10⁹ membership entries) | 35,920,666 | 30,217 | **2.06–2.17 GB** |
| TreeOfLife-200M (two views, seven levels) | 233,055,986 | 1,001,193 | **15.96 GB** |

Measured; `docs/ingest-campaign.md:1035`, `:1214`. The unit cost is measured too:
`probes/2026-08-16-membership-residency/` gives **78.5–94.0 bytes per Roaring container**, flat
across three decades, a **6.2× (contiguous) to 7.8× (scattered) ratio against serialised bytes**.
`artifact-serving-at-scale.md` §5.1 gives the row-major alternative at ~4 GB (label) and ~8 GB
(list) at 10⁹ rows whatever the artifact count, and notes the artifact-major form is still held
beside it (⊘ "the residency half of §5.1 is not taken here").

The campaign's own conclusion, measured, is worth quoting because it is exactly the mapped/resident
line: "the value columns and postings behave exactly as designed — mapped, resident only where a
request scans… **that part of the design already tolerates a bundle larger than memory. What does
not yet tolerate it is a fixed, per-process anon floor at open**" (`ingest-campaign.md:1214`).

### Per session, resident

- The frozen fragment: mapped from `.frag`, or held in the in-memory slot after a build.
- The row projection: `RowProjection::new(fragment, permutation)`, keyed
  `(token_id, view, segments_version)`. **125.12 MB dense at 10⁹** (`config.rs:1928`), ~2 B ×
  visible where sparse. `refresh.rs` measures the build at **1,277 ms at 10⁹**.

### Where a disk saving would and would not cost latency

**Would not cost latency:**

- `pairs.parquet`, `reports/` — no reader at all.
- The external-ID sidecar on a deployment that answers no drill-down and does not ingest: not
  mapped, not verified, not touched. Removing it removes bytes and nothing else.
- `entities/terms/` on the same deployment: mapped but never probed. 8.00 B/item.
- `row-entity.u32` on a deployment that serves no filter: 4.00 B/item per view.
- Anything that shortens the **open**'s digest sweep is pure win in time, since the bytes are read
  through a 1 MB buffer and dropped.

**Would cost latency, measurably:**

- Anything on the viewport path. `columns.arrow` and `morton.u32` are indexed by row id
  (`architecture.md` §10.3: "the row ID is the array index. Nothing is stored to locate row *i*")
  and read by the gather and the tile binary search. Making them smaller by any means that adds an
  indirection converts an mmap slice into a decode.
- `blocks.bin` on a corpus that serves drill-down. It is already zstd (§5); the cold-versus-hot
  gap is what a further saving trades into. Measured at rung 4: hot p50 at zoom 0 is **115.8 ms**
  and cold p50 is **7.27 s** for the same principal — the cost of a page that must be faulted
  (`ingest-campaign.md:895`).
- The artifact stores. Shrinking a `.tsmb` does not shrink the resident row form built from it at
  open; that is a 6.2–7.8× expansion into the heap and it is the term that OOM-kills, not the
  file. A disk saving there moves nothing unless the resident form goes too.

---

## 5. The compression question

### Compressed today

| file | scheme | measured ratio |
|---|---|---|
| `attrs/record/blocks.bin` | **zstd level 3**, 256 KiB uncompressed block target, a row never split (`tessera-filter-write/src/record.rs:63`, `:169`) | **0.342** on prose (PaperSeek 44.34/129.64 GB; MedCPT-10M 3.72/10.83 GB), **0.140–0.156** on structured rows (TreeOfLife-1M, gbif-64p). Measured here from `directory.arrow`'s own compressed/uncompressed columns |
| `terms/pairs.parquet` | Parquet 2.0, `DELTA_BINARY_PACKED`, **SNAPPY** (`store/src/pairs.rs:60–65`) | contracts §2.4 quotes 3.8× smaller than Arrow IPC |
| `attrs/<col>/dict.bin` | **front-coded blocks with periodic restarts** (`tessera-filter/src/dict.rs:1`) | not measured here |
| every `*.roaring`, every posting record, every `.tsmb` blob | CRoaring portable, run-optimised | structural, not a byte-stream codec |

### Stored raw, and the barrier for each

| file | barrier |
|---|---|
| `columns.arrow` | **Random access at a row id.** `architecture.md` §10.3 fixes it: "One file per column per segment: raw little-endian, fixed-width, uncompressed… the buffers remain page-aligned raw arrays that can be mmap'd and sliced zero-copy." The gather writes directly out of the mapping into the response's Arrow arrays (§10.4). Any block codec makes a row lookup a decompress. |
| `morton.u32` | **Binary search inside the mapping.** `tile_ranges` is `partition_point` over the mapped `u32` slice (`read.rs:1931`), and the underlay does up to 16,384 such walks per request (`read.rs:1914`). The column is sorted and would delta-code very well; the barrier is that a search needs random access at an arbitrary index. |
| `permutation.bin` | **Entropy, deliberately.** `architecture.md:225`: "the values inside a page are uncompressed precisely because entity order and row order are unrelated, making them maximum-entropy". The paging already removes the entity space a view does not occupy. `deferred-signature-major-layout.md` states the condition under which this changes: a signature-major row layout would make the slots near-monotone within a group and "Elias-Fano-class encoding becomes worth having". That sketch is explicitly not approved. |
| `row-entity.u32` | Random access at a row id, on the per-row filtered path. `row_entity.rs:19` measures the alternative (recovering the entity by inverting `tessera_id`) at **17.5 ns/row against 0.4 ns**, which is why the file exists at all. A codec would land on the same side of that measurement. |
| `entities/terms/{offsets,terms}.u32` | Random access at a has-row rank; `offsets[r]..offsets[r+1]` is two reads at arbitrary indices. |
| `attrs/<col>/values.arrow` | Sequential scan under a candidate mask, but the addressing is positional: "where every entity carries a value the entity id *is* the array index" (contracts §2.4). Compression would reintroduce the addressing structure the layout measurement removed (`probes/2026-08-08-filter-layout/`). |
| `entities/external-ids-*.arrow`, `ext-locator.u32` | **Ruled, not blocked.** Contracts §2.4: "**Compression is deliberately not used here**… the store is 18.6 GB of *disk*, never resident on the render path, so a block format, per-block digests and a decoder on an authorisation-adjacent path would buy disk for no latency gain; and it is machinery invested in a component scheduled for replacement." The threshold is stated: once mean external-ID length exceeds ~16 bytes the store dominates the bundle, and at that point compression or the replacement store pays. Random UUIDs do not compress. |
| `*.tslb`, `*.tsti`, `*.tscp` | Random access at an ordinal or a row, plus the packed-narrow-integer form is already the compression: `.tslb` is one integer of `row_column_width(ordinals)` bytes per row. |
| `MANIFEST.json` | It is digested by `CURRENT` and parsed at open. Nothing prevents compressing it; the barrier is that its size is a vocabulary problem, not a format problem — 34 MB of the geonames manifest is inline vocabulary values. |

The organising rule that produces almost all of the above is in `architecture.md` §10.3: **route by
access ratio**. Per-mark data goes in a fixed-width hot column (raw, mmap'd); per-query data goes in
entity space behind the filter contract; per-interaction data goes in a cold sidecar. The record
blob is compressed precisely because it is per-interaction; everything read per mark is not.

---

## What I could not establish

- ⊘ **No serve process was run.** I was blocked from starting one, so every resident/mapped figure
  here is either read from code or quoted from an earlier probe on an earlier bundle. In
  particular there is no measured RSS for a format-8 bundle.
- ⊘ **Four of the seven bundles measured are at a stale `bundle_format`** (3 or 5 against the
  reader's 8) and today's `open_bundle` would refuse them. Their per-item decompositions are still
  the right evidence about schema shape, but a figure from them is not a figure from a bundle this
  binary serves. Only `gbif-64p`, `treeoflife-1m` and the probe's GBIF listing are current.
- ⊘ **The fragment cache's growth rate was not measured**, only read from code. The claim that a
  new `.frag` pair appears per grant set per flush follows from the watermark being in the
  canonical key and from the sweep being fold-only; it has not been observed on a running,
  ingesting deployment.
- ⊘ **`dict.bin`'s front-coding ratio was not measured.** TreeOfLife's `uuid` column is 34.5 MB of
  `dict.bin` for 1M 36-character UUIDs, i.e. ~34.5 B/key against 36 raw, which suggests it is
  near-nothing on that input, but I did not decode the file to confirm.
- ⊘ **Contracts §2.4 says of the attribute postings "the postings are emitted and not read at
  serving".** That is stale: `filter.rs:2718` (`resolve_union` for `eq`/`in`), `filter.rs:2782`
  (the `derived` visibility gate's member sets) and `filter.rs:3461` (`text_match`) all read them
  on the request path. On PaperSeek that is 193 B/item, 27.8% of the bundle, so the discrepancy
  matters to anyone sizing from the document rather than the code. Worth an issue.
