# The filter-result cache, and what the text family still owes

**Date:** 2026-08-14 · **Status:** Handover memo — evidence, not normative. Names open work; rules
nothing. One item (§2) is **unruled and must not be built before it is**.
**Reads with:** [`records-and-search.md`](../../design/records-and-search.md) (r8),
[`filter-surface.md`](../../design/filter-surface.md) §4 (the *superseded* shared cache — read the
⊘ block before designing a new one), [`filter-index.md`](../../design/filter-index.md) §2.2,
[`architecture.md`](../../design/architecture.md) Appendix C rows C24–C26, decisions
[0063](../../decisions/0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
[0067](../../decisions/0067-term-timing-is-accepted-for-text-and-keyword-postings.md),
[0070](../../decisions/0070-analysers-are-named-and-declared-per-column.md), and the campaigns in
[`2026-08-13-text-index-bytes/`](../../../probes/2026-08-13-text-index-bytes/) and
[`2026-08-14-hidden-vs-absent/`](../../../probes/2026-08-14-hidden-vs-absent/).

The text family is built end to end: the declaration, the icu4x analyser, the token index through
all three writers (batch build, flush, compaction fold), `match` with its m-of-n form, exact phrase
verified against the record blob, and a conformance differential against an oracle that holds the
fixture's own prose. Merged to `main` at `e75b28e`.

What follows is everything that is not done, with enough context to resume without reading that
session's history.

---

## 1. Read this first — three things that will cost you a day

**1.1 A cache on the masked path is a disclosure surface, not an optimisation.** §2 below is a
cache proposal, and the single thing that makes it safe or unsafe is whether its key covers the
*overlay*. Read §2.2 before writing any of it. `filter-surface.md` §4.1–§4.5 specify a **different**
cache that is superseded and marked *do not implement*; its premise — that an operand is evaluated
unmasked and is therefore principal-independent — has been false since `filter-index.md` §2.2 made
the mask the scan's own candidate. Do not resurrect it by accident: the two proposals look alike
and differ in exactly the place that matters.

**1.2 An engine-level test cannot reach the ingest path.** Every engine test constructs an
`UnallocatedRow` directly and hands it to `accept_ingest`, which bypasses the Arrow batch parse. A
text column could not be ingested over `/control/ingest` **at all** — every batch carrying prose was
refused as a type error — and nothing in 1,300 Rust tests saw it. It surfaced the moment the column
was added to the conformance corpus, whose fixture drives the real HTTP surface. If you add a
column to a family, add it to `oracle/catalogue.py`'s `SCHEMA_TOML` early rather than last; that is
what exercises the wire.

**1.3 The measurement contradicts the reasoning more often than is comfortable.** Three times in
one session an obviously-correct change measured worse and had to be reworked, and each is recorded
where it was made rather than in a changelog:

- Starting a conjunction's running set as `candidate.clone()` cost **~800 ns**, most of a one-word
  query's whole budget at 10⁶ entities. The candidate is borrowed for the first step now.
- Chaining the out-of-place narrowing — the natural expression of the accumulator idea — measured
  **19–51% slower** than the route it replaced on 4- and 8-word conjunctions. In place, the same
  queries are 32% and 61% *faster*.
- The rewrite then **re-measured a leak-register row downwards** (C25: +155 ns → +22 ns at the
  singleton stratum, 88× → 24× at the head) and **reversed the sign of one of the campaign's own
  findings**. Anything that changes the filter's evaluation shape invalidates C25's figures; re-run
  `probes/2026-08-14-hidden-vs-absent/` and correct the register rather than leaving them.

---

## 2. The filter-result cache — designed, unruled, not built

**Owner's proposal (2026-08-14): an LRU over filter result bitsets, keyed on the filter and the
asking principal's term set.** The instinct is sound and the reasoning below is the analysis it
prompted, not a ruling. **The owner has not ruled it. Write the design note first.**

### 2.1 Why the win is larger than the general argument

A filter's result is computed in **entity space, before the tile sweep**, and only then crossed
into row space using the request's own ranges (`viewport.rs`, the `req.filter` block —
`evaluate_routed` then `cross_filter_into_row_space`). So the expensive half is
**viewport-independent**: a viewer who applies a filter and then pans or zooms re-evaluates the
identical expression on every frame.

That is the argument to make. The generic "queries repeat" case is weak and was already rejected
once; this one is a property of the product's own interaction.

**⊘ It applies to the entity route only.** Decision 0068's row route evaluates a leaf over the
request's own rows, which are viewport-dependent, and `evaluate_routed` picks between them per
request on `rows_in_ranges ≤ v_total`. So the same filter takes different routes on different
frames. Cache `RoutedFilter::Entity`; a frame that takes the row route simply does not hit, and it
took that route because it was cheap for that frame.

### 2.2 The key, and the part that is a fail-open if omitted

The candidate a filter is evaluated under is built by `filter::candidate` and is

> `fragment.view() ∖ overlay.denied()`, plus every buffered entity whose verdict admits it

so the answer depends on **four** things, not two:

| component | why it must be in the key |
|---|---|
| the canonicalised filter expression | the question asked |
| the **satisfied term set** | what makes sharing across principals sound at all — two sessions with the same grants have the same visible set, which is the whole of the owner's idea and is correct |
| `segments_version` **and** `prefix` | a flush publishes entities; the same filter over the same term set has a different answer. `RowProjectionKey`'s doc argues why the prefix is not redundant with the version |
| **`overlay_version`** | **the one that is a disclosure if missed** |

**The overlay moves without the bundle moving.** A suppression takes effect the instant it is
accepted and publishes no segment, so `segments_version` does not change. A key that omits the
overlay therefore **keeps serving a suppressed item from cache**, silently, presenting as an
improved hit rate — which is exactly the fail-open Rule S exists to prevent and exactly the shape
write-path §5.4 warns has been reached twice in review by other routes.

`Generation::overlay_version` exists for this: a monotone counter bumped on every overlay or buffer
swap, deliberately independent of `segments_version`. There is precedent for folding it into a key
— the delta-serving content keys do, alongside `Engine::boot_nonce`, whose doc explains the
restart-collision case that nonce answers. An in-memory cache dying with the process does not need
the nonce; read that doc anyway before deciding it does not.

**On canonicalising:** key on the parsed `FilterExpr` serialised deterministically, and do **not**
try to prove that `all_of[A,B]` equals `all_of[B,A]`. Missing that costs hit rate; getting it wrong
costs correctness. `filter-surface.md` §4.3's "canonical level-tree node identities" were cut with
the rest of that section and also addressed a structure `filter-index.md` §3 removed.

### 2.3 Cache the leaves, not the tree

The owner's phrasing reads as whole-expression. **Leaf-level is better**, for three reasons:

1. **The leaves are the cost.** A posting read is microseconds (C25's own figures); recomposing a
   tree is bitmap arithmetic at container rate.
2. **Different filters that share a clause share entries**, which whole-expression keying cannot do.
3. **It is the only version that helps [#121](https://github.com/jennis0/tessera-index/issues/121).**
   That issue is an expression repeating one clause thousands of times, each repetition paying full
   cost. Leaf caching collapses them to one evaluation and changes the amplification arithmetic
   materially. Whole-expression keying leaves #121 exactly where it is.

A leaf's cached value is `leaf ∩ candidate`, so it carries the same key structure — more entries,
better reuse.

### 2.4 What to reuse, and what to write

**Reuse `SingleFlightCache` (`engine/src/single_flight.rs`) and model the wrapper on
`RowProjectionCache` (`engine/src/cache.rs`).** It already carries the byte bound, LRU eviction,
the single-flight state machine (without which N concurrent identical requests all miss and all
compute), and two pruners for entries whose generation no request can name. `cache.rs`'s module doc
and `RowProjectionKey`'s field-by-field argument are the model for what a new key's doc owes —
including the note on why the key is a struct with named fields rather than a tuple of `u64`s, a
transposition there being cross-principal mask reuse that compiles and runs.

**Seam:** `FilterColumns::resolve` is per-leaf and per-column and takes the candidate; `eval` walks
the tree. The cache belongs above `resolve` and below `eval`, which means it needs the key's
generation and overlay components threaded in — they are on the `Generation`, which `resolve` does
not currently see. That threading is the bulk of the work and is worth designing before typing.

### 2.5 The tests that would make it safe

Three, and the first is the one that matters:

- **A suppression invalidates.** Filter, suppress a matching item, filter again with the same
  expression and the same principal — the item must be gone. Mutation: remove `overlay_version`
  from the key and this must fail. Without that mutation check the test proves nothing.
- **A flush invalidates.** Same shape over `segments_version`.
- **Two principals with the same term set share; two with different term sets do not.** The second
  half is the cross-principal check, and `eviction_never_widens_a_mask` in `cache.rs` is the
  existing precedent for what such a test looks like.

### 2.6 One channel to register rather than discover

A shared LRU means one term set's traffic evicts another's, so hit-or-miss timing weakly signals
that some other term set is active. Very weak beside C24 and C25, which are already accepted — but
this corpus's habit is to name a new surface rather than notice it later, and Appendix C is
exhaustive only if new surfaces are named.

---

## 3. The text family's remaining work

### 3.1 The epic's own fourth gate is not discharged — the layered/folded conformance

Epic [#86](https://github.com/jennis0/tessera-index/issues/86) asks for the text fold to be proven
by *"the folded-against-layered differential"*. The fold is tested hard at **engine** level
(`crates/tessera-engine/tests/fold_text.rs` — five cases including the flight-carry and the
empty-batch skip), which opens the artefacts directly. **Conformance covers the base build only**
(`conformance/tests/test_text_differential.py`, module doc says so).

The shape to copy is `conformance/tests/test_keyword_layers.py`: drive the corpus over the control
plane to four states — base, base + one flush extent, base + two, then `POST /control/compact` —
and ask every probe at each. **Those four states already exist and already carry text extents**,
because the fixture's ingest batches now plant prose; nothing asserts the answers. So the one place
a cross-layer text defect would surface is the one place nobody is looking.

Roughly an hour. It is also where *"a search returns the same items before and after a
compaction"* becomes a property of the service rather than of the merge function.

### 3.2 The four filed issues

| # | What | Bite |
|---|---|---|
| [122](https://github.com/jennis0/tessera-index/issues/122) | Nothing coalesces a text layer, so `match` pays one dictionary resolve and one posting read **per flush** until the nightly fold | The real one for a busy deployment. The merge already exists — `compact.rs`'s `merge_text_layers` with an empty tombstone set — so it is mostly plumbing plus a presence union for the coalesced layer |
| [123](https://github.com/jennis0/tessera-index/issues/123) | No way to ask whether an item carries prose at all, which is why `none_of` over a text column is **refused** | Small. The base index must write a presence bitmap and all three writers must agree; that is a format change, free pre-release under 0048 provided artefacts are rebuilt |
| [124](https://github.com/jennis0/tessera-index/issues/124) | The analyser identity is in the manifest and not on `/v1/meta` | Tiny — one field. A client whose CJK query returns nothing currently cannot tell whether its query segmented differently from the index |
| [125](https://github.com/jennis0/tessera-index/issues/125) | The recorded analyser version does not cover **std's** Unicode tables, which `char::is_alphanumeric` consults and which move with the toolchain | Small, low exposure. Take the property from `icu_properties` — already in the tree and pinned — so one version claim covers the whole pipeline. Changing the token rule needs `p1` → `p2` and a rebuild |

`Cargo.toml` pins the three icu4x crates at `=2.2` so a bump must be a deliberate edit that moves
`UNICODE_VERSION` with it. That half of #125 is closed; the constant's doc says which half is not.

### 3.3 Two loose ends, neither pressing

**The fold's memory pre-flight does not count the text dictionary's spool buffer.** `memory_estimate`
charges 8 B per *term-dictionary* ordinal only; a text column's vocabulary is a separate and smaller
number (991k terms over 2.4M abstracts, measured) and the estimate's ×2 safety factor covers it at
every scale measured. A deployment with several wide prose columns would want it counted. Marked at
the site in `compact.rs`.

**Appendix A's sizing tables do not list the text artefacts.** `records-and-search.md` §13 marks it
owed; it is a documentation task.

### 3.4 Two tests are load-flaky, and neither is text

`dropping_a_client_connection_mid_viewport_releases_the_gate_promptly` and
`no_permit_leak_after_a_shed_or_a_completion`, both in `crates/tessera-server/tests/http.rs`. Both
measure in-flight request counts under timing bounds. Both pass in isolation every time and the
whole file passes at `--test-threads=4`; under a full-parallel `cargo test --workspace` one fails
perhaps one run in three. Pre-existing and unrelated to this work — but CI cannot rely on a clean
full-parallel run until they are fixed.

---

## 4. Not yours to decide

**[#121](https://github.com/jennis0/tessera-index/issues/121) needs the owner's ruling** and is
open at the time of writing. A filter expression's *breadth* is unbounded where its depth is capped
at four, so one request may repeat a clause thousands of times and pay full cost each time — which
multiplies the timing channel C24 and C25 accept at severity *low* on the strength of those being
single-shot differences.

The measured single-shot separations are +22 ns (a rare word) to +10.9 µs (a word a sixth of the
corpus carries). Two thousand clauses makes the first a third of a millisecond and the second
twenty-two seconds.

The recommendation in the issue is a node count checked at parse beside the existing depth limit,
**plus** a sentence in the register saying the multiplier exists — both, not either. §2.3 above
notes that a leaf-level cache would also blunt it, which is a reason to have that design note in
hand before the ruling rather than after.

---

## 5. Verifying

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
python3 scripts/check-doc-links.py
reference/.venv/bin/pytest conformance/tests -q      # 347, against a real `tessera` subprocess
```

Green on `main` at `e75b28e`: 1,339 Rust tests, 347 conformance, clippy clean, layering clean, no
broken doc links.

**`cargo fmt` is not part of the gate and the tree is not rustfmt-clean** — 80 files differ at
HEAD. Running `cargo fmt --all` produces a large unrelated diff; format only what you touch, or
nothing.

Two probe harnesses re-run the figures this memo cites, both wanting `--snapshot` pointed at the
arXiv v296 snapshot:

```bash
cargo run --release --manifest-path probes/2026-08-14-hidden-vs-absent/hiddentiming/Cargo.toml -- \
    --snapshot ~/.cache/kagglehub/.../arxiv-metadata-oai-snapshot.json --scales 400000,1000000
cargo run --release --manifest-path probes/2026-08-13-text-index-bytes/textbytes/Cargo.toml -- \
    --snapshot ~/.cache/kagglehub/.../arxiv-metadata-oai-snapshot.json
```

Both call the **shipped** route through `FilterColumns::resolve` and the shipped writers. That gate
is stated three times across these campaigns because it has been broken twice: the `utf8`
retirement fence measured a route that did not exist, and the `contains` recovery probe repeated
the mistake. A harness that reimplements the thing it measures measures nothing.
