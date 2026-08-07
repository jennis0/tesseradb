# Compaction review — fidelity and implementability lens

**Status:** Evidence — review transcript, never normative. One independent review of
`docs/design/compaction.md` (r2, provisional, drafted 2026-08-05); the reviewer did not write the
draft and has made no edit to it or to any corpus document. Findings are **not** dispositioned —
that is the owner's, per `docs/agents/design-process.md` §4.

**Lens:** the failure mode this lens caught on `write-path.md` r2 — a stale claim about the tree or
about a contract, laundered into a new document as fact. Nothing in this design is built, so every
load-bearing sentence is either a claim about what exists or a claim about what the contracts
permit. Verified against the working tree at `673c267` on `geometry/cell-plus-residual` (which
includes the untracked `crates/tessera-engine/tests/scale.rs`,
`crates/tessera-store/tests/merge_props.rs` and `probes/2026-08-05-write-path-at-scale/`).

Findings are ranked. Each says whether the design survives correcting it.

---

## Findings

### F1 — pass 3's "full-length `ext-locator.u32`" silently breaks drill-down for every entity ingested during the fold. Fatal; the design does not survive as written.

Spec §3, pass 3: *"a scatter of `entity → ordinal` into a memory-mapped full-length
`ext-locator.u32` with `0xFFFFFFFF` for entities that never had a key"*, while §2's table carries
post-snapshot runs forward.

The sidecar resolves in this order (`tessera-store/src/sidecar.rs`,
`ExternalIdSidecar::external_id_of_checked`):

1. `if entity.raw() < self.locator_len() { return self.external_id_of(entity) }` — **the base
   locator wins outright** for any entity inside its length, and `external_id_of` treats the slot
   as a position in the *listed-order concatenation of every run*, walking `scan.bounds`.
2. only *past* the base locator's end is `locator_runs` (the carried-forward `locator_extents`)
   consulted, each of which names its own run and is run-local.

The module states the two conventions and why they differ, at `LocatorRunDesc`'s doc: *"not into
the concatenation of every run, which is what the base locator's ordinals mean."*

So if the fold's new base locator is sized to the **live** entity space and writes the sentinel for
post-snapshot entities, then for every entity flushed during the fold `external_id_of_checked` takes
branch 1, reads `0xFFFFFFFF`, and answers `Ok(None)` — *"this item has no external id"* — while its
key sits in a carried-forward run that branch 2 would have found. That is precisely the outcome
contracts §2.4 names as forbidden: *"Reading past the array and returning 'no external ID' for an
item that has one is a wrong answer wearing a legitimate state's clothes."* It is silent: no error,
no counter, and the item is still visible on the map.

The fix is one sentence in the design, and the draft has to choose which: either the fold's base
locator is bounded by the **snapshot's** entity space (so post-snapshot entities fall past it and
take the extent branch — the same shape a build's locator has, contracts §2.4's *"length
`entity_id_high_water` at build"*), or the fold computes true global ordinals for the
carried-forward runs and the carried `locator_extents` become dead weight. The first is almost
certainly intended; "full-length", paired with "for entities that never had a key", reads as the
second and is what an implementer would build.

Note this is the same class of hazard write-path §7 already records on the other side — *"the base
locator needs no repair: its ordinals all resolve inside run 0, which no merge ever consumes,
provided run 0 stays listed first"* — and the fold is the one operation that rewrites run 0.

### F2 — §4 step 5's cost claim is false, and no in-process second-prefix open exists. Fatal to fidelity; the design survives, at a cost it must now state.

§4 step 5: *"Open the new prefix in-process — not `Engine::open`. Mapping the new files is lazy;
what costs anything is rebuilding the carried-forward extents' row maps from their own `tessera_id`
columns (contracts §2.6 r16), bounded by those segments' row counts, which are one tick of ingest
each."*

Two things are wrong.

**There is no such entry point.** `tessera-store`'s three incremental constructors —
`Bundle::with_segment`, `with_merged`, `with_manifest` — all route through `substituting`, which
clones the partition maps and keeps `manifest: self.manifest.clone()`. None can replace the
top-level `Manifest`, the base `Permutation`, or the prefix. A fold needs all three. The only
whole-bundle route in the tree is `open_bundle`.

**`open_bundle` is not lazy.** It runs `verify_files(&prefix_dir, &manifest.files)` — which reads
every named file in `DIGEST_CHUNK_BYTES` chunks and SHA-256s it — and
`load_verifying_segments_manifest`, which does the same for the side-manifest's own `files` map.
Between them that is *every* byte of the bundle (the memo the draft cites puts a 10⁹ bundle at
47 GB). It then calls `Permutation::validate_rows(row_count)` once per slice, which allocates
`vec![false; row_count]` — 1 GB of anonymous memory at 10⁹ rows — and scans all `bound` slots,
faulting in the whole 4 GB permutation. `Bundle::with_segment`'s doc says this in as many words:
*"`open_bundle` maps every file afresh and `Permutation::load` re-pays an O(bound) `validate_rows`,
so re-opening the bundle per flush would cost more than the flush it followed."*

So step 5 is either (a) `open_bundle`, which costs a full re-hash and a 1 GB transient **on the
executor**, between the `CURRENT` flip and the swap — blocking every deny and every ingest for the
duration; or (b) a fourth `Bundle` constructor that takes the fold's own freshly-written, freshly-
digested artefacts on trust and skips verification. (b) is the right answer — the fold wrote the
bytes and knows their digests, exactly as `with_segment`'s doc argues for a flush — but it is a new
`tessera-store` API that deliberately bypasses the reader protocol, and **§10 assigns no such work
to `tessera-store`**. It also needs saying which verification a fold-constructed bundle skips, since
the answer today is "all of it, and the next restart re-does it".

### F3 — the draft contradicts `architecture.md` §11.3, which wins, and never says so. Significant.

Architecture §11.3, unqualified and unmarked: *"A compaction rewrites the permutation and the
columns, publishes them under a new segment-set version, and lets in-flight requests drain (**I11**).
At single-node scale it does **not** invalidate the term index, masks or generating sets."*

The draft's §4 and §6 are the exact negation: base postings are rewritten, `bundle_identity`
rotates, and *"Fragments must be rebuilt too, because the identity rotates"* (§6). Fragments are
masks.

The draft is almost certainly right and the specification stale — §11.3's own r33 ruling
(*"only a post-fold fragment stops containing the entity"*) makes the older sentence untenable, and
write-path §5.4 was written on the newer reading. But `architecture.md` is the specification and
wins over everything, and the sentence has not been amended. A promotion that leaves it standing
gives the corpus a top-authority statement that a fold need not invalidate masks, which is the
fail-open r33 exists to close. The draft should name the sentence and the amendment it owes; today
it names only `publish_geometry`'s doc comment, which is downstream of it.

### F4 — contracts §2.1 and §2.6 both say every compaction emits exactly one segment per partition-slice. An in-process fold cannot. Significant.

Contracts §2.1: *"`tessera build` and every compaction emit exactly one segment per partition-slice
— a build **is** a full compaction. … **This is what makes the slice-level `permutation.bin`'s
single-segment addressing (2.6) sufficient.**"* Contracts §2.6 closes with *"Compaction folds
everything back into one segment and a fresh slice-level permutation."*

The draft's §1 refuses to block flush (*"Ingest, denies and flush continue … a flush publishes into
the old prefix and is carried forward at the flip"*) and its §2 table carries post-snapshot segments
forward as extents. So the fold's output prefix is `base + k extents`, not one segment, whenever
anything flushed during a fold that runs for *"minutes-to-hours"* (§3). The construction is sound —
`RowSpace` is base-plus-extents by design and `with_extent` accepts `entity_lo >= base.bound()` — but
the contract sentence is false of it, and it is a sentence the contract explicitly leans on.

The draft's own §0 obligation 3 repeats the contract's claim — *"the fold returns the bundle to one
segment per partition-slice, one base postings tier, one external-id run and one locator —
contracts §2.1's 'a build is a full compaction', reached without a build"* — and is therefore in
direct contradiction with its own §2 table. One of the two has to give: either §0's third
obligation is restated as "one, plus whatever landed in flight", or the contract is amended, or the
fold quiesces flush (which §1 rules out).

### F5 — §6's pre-swap warm is not "the same pass at a different point", and the "2× the byte budget" does not follow from the cache the draft is describing. Significant.

Two independent problems, both in existing code.

**`refresh::refresh_resident` cannot cross a prefix.** Its first statement per key is
`if key.prefix != generation.prefix || key.segments_version >= generation.segments_version {
continue; }`. Across a fold, `key.prefix` differs for *every* resident entry, so the pass produces
zero. §6's *"Pre-swap is the same pass at a different point"* is not true of the pass that exists;
the prefix filter must come out or a second pass must be written, and either is a change to the one
mechanism decision 0044's D1 rules.

**The cache has one byte bound, and its miss path evicts to fit.** `RowProjectionCache` wraps
`SingleFlightCache`, which holds a single `bound_bytes` and, at publish, *"evict[s] to fit, then
publish[es] `Ready`"* in LRU order. Inserting a new-prefix entry beside each old-prefix entry
therefore does not give *"peak residency … 2× the row-projection cache's own byte budget"*; it
evicts the old-prefix entries — the ones still serving live requests against the still-live old
geometry — one per insert once the bound is reached. Each such eviction charges some live session a
rung-3 rebuild (measured 4 550 ms) *during* the fold, which is the cost §6 exists to avoid, moved
earlier. Making §6's claim true requires either raising the bound for the fold's duration or
holding the pre-warm outside the bounded cache; §10 assigns neither.

A smaller correctness note in the same section: the draft says rung 2 (stale-serve) *"is unsound"*
for a fold. It is, but the reader should know the mechanism is already stronger than that —
`RowProjectionKey` carries `prefix`, and `session_geometry` builds its `stale_key` by cloning the
live key, so after a fold rung 2 structurally misses rather than being wrongly taken.

### F6 — a fourth seam gap the draft does not name: `prefix_dir` is bound for the process lifetime on both the `Engine` and the executor. Significant.

§4 names *"Three gaps must close in this change"*. There is a fourth, and it writes into the tree
§8 is about to delete.

`Engine.prefix_dir` is a plain `PathBuf` set in `Engine::open` (`session.rs`), cloned into
`MaintenanceDeps` and thence into `Executor.prefix_dir`, whose own field doc reads *"The bundle
prefix directory a flush writes into. A flush publishes **inside the current prefix** … which is
what separates it from a compaction."* Every flush, merge and coalesce publication resolves paths
and calls `write_segments_manifest` through it, and `ExternalIdIndex::open` is handed it too. Nothing
reads `Generation::prefix` for this purpose.

After a fold flips `CURRENT`, the very next flush writes its segment and its `SEGMENTS-<n>.json`
into the **old** prefix — which §8 then unlinks whole, taking an acked, published flush with it, or
(worse) which the sweep leaves standing while `CURRENT` names a prefix whose manifest never
mentioned it. This is not a refinement of gap 3 ("the signature widens"): the signature carries what
is published, and `prefix_dir` is what the *next* publication writes through.

### F7 — §10's `tessera-build` row requires a new production edge from the engine to the build pipeline. `check-layers.sh` would stay green, which is the problem. Significant.

§10 assigns `tessera-build` *"the `pairs.parquet` writer, reused as pass 2's side output rather than
reimplemented"*, and §10's closing line says *"`scripts/check-layers.sh` is unaffected: the engine
already depends on both store and authz, and the fold adds no publisher."*

Both halves of that line are true and neither is the point. `cargo tree -p tessera-engine -e normal
--depth 1` lists authz, lifecycle, plugin, spatial, store, types — **not build**. `tessera-build` is
a *dev*-dependency of `tessera-engine`, and `parquet` is a normal dependency of `tessera-build`
alone (it is a dev-dependency of engine, server and cli). `PairsParquetWriter` is `pub(crate)`.

So the assignment needs a new normal `tessera-engine → tessera-build` edge, which drags the offline
build pipeline — `input.rs`'s Parquet reader, `pipeline.rs`, the whole batch build — into the
serving binary, and inverts the layering (`tessera-build` sits *above* store and authz and is not a
serving component). `check-layers.sh` has no `deny tessera-engine tessera-build` rule, so it will not
notice. The alternatives are to move the pairs writer down into `tessera-authz` (which §10 already
says owns postings, and which is where the term sweep lives) or to accept the edge explicitly with an
argument. Either way the draft's "unaffected" sentence is answering the wrong question.

### F8 — the central memory claim is not O(1) in corpus size at two named places, both in existing code, and one of the two mechanisms it cites does not exist. Significant.

§3: *"the memory rule this design is built around: peak RSS is O(1) in corpus size"*, and pass 1:
*"`permutation.bin` is 4 B × (max folded entity + 1) — 4 GB at 10⁹ — and is *written through a
mapping*, so it is page cache rather than RSS, exactly as `Permutation::load` already treats it at
read."*

- **There is no mmap permutation *writer*.** `Permutation::load` mmaps at read, correctly cited. The
  only writer is `write::write_permutation_iter`, which does `let mut slots =
  vec![PERMUTATION_ABSENT; bound_usize];` and whose own doc calls it *"4 GB at 10^9 entities. That is
  inherent to the format (R4 …) and is the ledgered, accepted cost."* "Exactly as … already treats
  it" reads as description; it is new machinery, and §10's `tessera-store` row does not list it (it
  lists the *locator* scatter, which is a different file).
- **`PostingsSpool` holds one `i64` per term.** `offsets: Vec<i64>` grows by one entry per
  `append`, and `finish` consumes the whole vector. At the corpus the draft designs against
  (1.17×10⁸ terms, §14) that is ~936 MB of anonymous memory held across the whole of pass 2. It is
  O(dictionary), not O(1), and the dictionary is a corpus-scale quantity.

Neither is fatal — both are bounded and both are already paid by the build — but §12's obligation 12
("Peak RSS is flat in corpus size") is the design's stated central claim, and as written it is false
of the machinery the design names. State the two terms and their sizes, or the probe will refute
the claim rather than confirm it.

Related, and worth one line since the same paragraph is the argument: *"the largest term …
~62 MB as the portable Roaring the tag-1 records already hold"*. A term covering 25–50% of a 10⁹
universe puts every one of the ~15 259 containers into bitmap form, which is ~125 MB, not 62. Still
O(1); the figure is out by 2×.

### F9 — "a sorted-item iterator feeding the one existing segment writer" is larger than an entry point, and the writer it would be built on cannot emit declared scalars. Moderate.

There are already two column writers behind one byte-level chokepoint: `write_segment(dir,
&[TilerItem], &[u32], &[(String, ScalarType)])`, which materialises the whole batch and *is* the one
that writes declared scalars; and `write_columns` / `write_columns_from_parts`, which take the two
fixed columns only and whose doc explicitly offers the mmap-backed handover the fold wants (*"at
3×10⁹ rows the two `Vec`s of `write_columns` are 36 GB of anonymous memory, whereas mmap-backed
`Buffer`s … cost address space only"* — an entry point that exists and that nothing currently calls
with a file-backed buffer). Both funnel through `write_single_batch`, so "one writer" is already
true at the bytes.

Consequences the draft should absorb rather than leave to the implementer:

- A streaming fold writer built on `write_columns_from_parts` **silently cannot write
  `declared_scalars`**. That array is empty in every bundle today (contracts §2.2's ⊘), but ingest
  validates against it and `flush` writes the tail through `write_segment`'s `scalar_schema`. A fold
  that drops it would produce a base segment whose schema disagrees with its own extents'.
- Conversely, a fold built on `write_segment` re-materialises the corpus as `Vec<TilerItem>`, which
  is the 4.4–4.9× multiplier §3 refuses to inherit.
- §3's framing — *"the k-way merge is a **sorted-item iterator feeding the one existing segment
  writer**, with `execute_merge`'s sort-then-write becoming the other producer of the same stream.
  One writer, two producers"* — describes a refactor of `write_segment` into a streaming form with
  the slice form on top of it. That is the right shape and it is real `tessera-store` work; say so.

### F10 — §3 pass 5 and §4 step 4 give opposite orders for `MANIFEST.json` and the hard links. Moderate.

Pass 5: *"`MANIFEST.json` for the new prefix, digesting each file as it is written …; **then** the
carried-forward files hard-linked in (spec §8)"*. Step 4: *"**Hard-link the carry-forwards → write
`MANIFEST.json`** → write `SEGMENTS-<n>.json` → flip `CURRENT`."*

This is not cosmetic. `CURRENT.manifest_digest` pins `MANIFEST.json`'s bytes, and the carry-forward
set is only known at publication, after the rebase in step 1. So MANIFEST cannot digest the
carry-forwards, and their digests must ride the new `SEGMENTS-<n>.json`'s `files` map — which
`ensure_verified` accepts (it checks either map) and which is what a flush already does. That is
almost certainly the intended design; pass 5's ordering says the opposite, and an implementer who
follows it will find MANIFEST unwritable on the fold thread.

### F11 — §4 step 3's premise is backwards: a fold ordinarily *grows* the base segment. Moderate; keep the check, drop the reasoning.

*"`max_merged_segment_bytes` must stay strictly below the base segment's bytes or the next startup
refuses the configuration (write-path §10) — and a fold shrinks the base."*

The relation is real and is checked where the draft says (`tessera-server/src/lib.rs`, after
`Engine::open`, only for an explicitly-set value). But it is computed as the **max over every
segment** of `columns.byte_len() + morton.byte_len()`, and the fold's defining act is to fold every
extent *into* the base. The single output segment is therefore normally larger than the pre-fold
base — a fold makes the relation easier to satisfy, not harder — and it shrinks only in a
deployment where folded deletions exceed the extents absorbed. Keeping the pre-flip check is right
(it costs nothing and covers the delete-heavy case); the sentence justifying it is wrong.

### F12 — pass 2's `pairs.parquet` side output contradicts the same pass's memory rule, and the file is optional. Moderate.

Pass 2 forbids materialising a posting as a `Vec<u32>` (*"Unions happen in the posting's own
representation, never as a `Vec<u32>`"*) and then adds `pairs.parquet` as a side output of the same
sweep. `PairsParquetWriter::push_run(term_id, entities: &[u32])` takes exactly that slice; the
per-row `push` alternative is the one the build measured and removed (*"only the 1.7 × 10⁹
call-per-row overhead is gone"*). So the largest term costs either the 1–2 GB `Vec` the paragraph
above forbids or the call overhead the build already rejected. Bounded either way, but it is the one
place the pass's own rule is broken by the pass's own new obligation, and the draft should say which
horn it takes.

Second, smaller point: `pairs.parquet` is optional at build (`tessera build --no-oracle-pairs`,
contracts §2.4 *"optional for a serving deployment, required for a conformance run"*). A fold over a
bundle that has none must not manufacture one — §14's evidence posture and the operator's explicit
choice both say so — so the rule is "re-emit iff the input prefix had one".

### F13 — §11's I10 row states the invariant backwards. Minor; the design survives, the table does not.

*"**I10** | … and no blinded identifier reaches any durable file."* `columns.arrow`'s `tessera_id`
column **is** the blinded identifier, it is durable by contracts §2.6, and the fold writes it. What
the rule actually says (contracts r6, and `check-layers.sh`'s `fn entity_id` grep) is that no
*request-path artefact* stores an **entity** id — and even that needs the qualifier, because
`external-ids.arrow` stores `entity_id: uint32` durably as an off-request-path sidecar. As written
the row asserts something false about a file the fold's own pass 1 emits.

### F14 — the fold is the first thing in the tree that ever mints a prefix name, and the draft does not say how. Moderate.

`tessera-build` has `const PREFIX: &str = "v00000"` and writes only that; nothing else in the tree
writes a `CurrentPointer.prefix`. Contracts §2.1 shows `v00042` in its layout example and specifies
nothing about the name. So the fold owns a naming scheme that does not exist, and the draft names
neither the scheme nor its width.

This matters for one of write-path §8's explicit obligations, *"the never-reused `seg_id`
namespace"*, which the draft's Appendix R lists as answered at spec §4. Spec §4 only *relies* on the
rule for its ABA argument; nothing discharges it. Today `seg_id`s are `flush-{n}-{attempt}` and
`merge-{n}-{attempt}`, unique across prefixes because `n` is monotone across prefixes — but the fold
writes its segment on the fold thread, *before* step 2 allocates `n`, so it cannot use that shape,
and after a crash `next_manifest_n` reseeds from the live prefix only (the orphan prefix is not
scanned). A re-planned fold can therefore re-derive both a prefix name and a `seg_id` an abandoned
fold already used, and §3's "failure discards, the next trigger re-plans from scratch" races §8's
orphan sweep. One paragraph fixes it; §8's obligation is currently mentioned, not discharged.

### F15 — §9 and §10 disagree about where the trigger and its gauges live, and one gauge cannot be computed where §10 puts it. Minor.

§9 evaluates the trigger *"at the flush tick, like every other cadence here"* — the write executor,
in `tessera-engine`. §10 assigns *"the three gauges"* and *"the free-space precondition"* to
`tessera-server`. The free-space precondition gates the fold, which is engine work. And the
`dead_bytes` gauge is "on-disc minus manifest-named", where "manifest-named" is store data:
`check-layers.sh` carries `deny tessera-server tessera-store` ("server sees engine API types only"),
so the server cannot read a `files` map. Both gauges have to be engine-side with a server accessor.

### F16 — §6's `x-tessera-stale` is not a broadcast. Minor.

*"`x-tessera-stale` flips to 1, as at any publication — broadcast, advisory."* It is computed per
request as `stale = stamp.is_some_and(|presented| presented != answered_from)`
(`viewport.rs`), so it is `0` for every client that presents no stamp. Advisory and never a refusal
is right; "broadcast" is not.

---

## Categories attacked that survived

**Task A — claims about the tree, each checked at the source.**

- `Engine::publish_geometry`'s signature is exactly `(prefix, segments_version, watermark, bundle,
  dict, delta_postings)`; the executor arm carries `postings: Arc::clone(&previous.postings)` and
  the live `overlay`/`buffer`/`overlay_version`, so base postings are genuinely not swappable
  through it; and the doc does call itself *"compaction-shaped"* in the sense the draft quotes.
- `bundle_identity` is `hex_decode_32(&current.manifest_digest)` at `Engine::open`;
  `FragmentCache::new(cache_dir, bundle_identity, auth_plugin_hash)` is constructed there and stored
  on `Engine`; the cache key is `SHA-256(bundle_identity ‖ auth_plugin_hash ‖ sorted term ids)` and
  names the persisted `.frag` file. Nothing rotates either in-process. §4's premise holds exactly.
- `Generation` carries `postings`, `dict` and `delta_postings` (with `prefix`, `segments_version`,
  `watermark`, `bundle`, `overlay_version`, `overlay`, `buffer`, `denied`). Correct — with the note
  that `external_index` is already an `Arc<ArcSwap<ExternalIdIndex>>` the executor stores into
  (`coalesce`'s publication does), so §4 step 6's external-id swap needs no signature widening; it
  is a choice, not a necessity.
- `PostingsSpool`: `create`/`append`/`finish`, records in term order with ordinal = term id, spool
  fsynced then mmapped as the `LargeBinaryArray`'s values buffer, one batch through the same
  `write_posting_array` as the buffered path. Exactly as described.
- Delta tiers: `DeltaTier::open` refuses a tier whose `term_id`s are not strictly ascending, at
  open rather than per lookup, and `posting_of` is a `binary_search` over `terms.values()`.
- Segments: `columns.arrow` is Arrow IPC, one record batch, uncompressed, `(tessera_id, residual)`
  plus the declared-scalar tail, mmapped; `morton.u32` is raw little-endian `u32`s with no header;
  the order is `(morton, tessera_id)` (contracts §2.6, `sort_batch`, and `write_segment`'s
  non-decreasing debug assertion). All as claimed.
- `Permutation` is mmapped at read (`Mmap::map`, `slots()` a zero-copy cast). As claimed — see F2
  for the `validate_rows` cost the claim is used to imply away.
- `execute_merge` concatenates every input into `Vec<TilerItem>`, re-sorts through `sort_batch`, and
  writes through `write_segment`, which takes slices. The *"a second writer that knows the layout is
  how two come to disagree"* quote is genuinely write-path §7's (and `execute_merge`'s own doc says
  the same). Correct.
- `RowProjectionCache` is byte-bounded with LRU eviction, over `SingleFlightCache`'s recency index,
  with `set_bound_bytes` called by `tessera-server` and unbounded elsewhere.
- The `max_merged_segment_bytes < base-segment-bytes` relation is enforced in
  `tessera-server::lib.rs` after the bundle opens, only for an explicitly-set value. Correct as to
  *where*; see F11 as to *why*.
- `open_bundle` reads `CURRENT` first; streamed segments carry no permutation file and their row map
  is rebuilt at open by inverting `tessera_id` (`SegmentExtent::rebuild`), which is also the
  inversion §11's I10 row cites.
- `tessera build` refuses a root containing `CURRENT` (`tessera-build/src/lib.rs`, with the
  overwrite argument in the comment).
- `PairsParquetWriter` is the only Parquet writer in the tree, `(entity_id: uint64, term_id: uint32)`
  sorted by `(term_id, entity_id)` — which is exactly the order pass 2's sweep produces. Correct;
  see F7 for where it lives.
- Two further claims not on the brief's list, both correct: `merge_enabled`/`coalesce_enabled`
  already exist as engine-level `AtomicBool`s, so §1's suspension has a lever; and `hard_link` is
  already used (flush's refuse-to-replace manifest write), so §8's hard-link carry-forward is
  available on the paths the store writes.

**Task B — contracts.** `n` continues correctly across a prefix: the executor's counter is seeded
one past the highest *candidate* per partition at open and is process-local thereafter, and
contracts §2.3 makes `n` monotone across prefixes. `deltas` order is immaterial (tiers are unioned).
`external_id_runs` recency-as-list-position survives: the fold replaces the *oldest contiguous
window* with one run 0 in position 0 and appends the carried runs after it, which is exactly the
rule §2.4 states. `locator_extents` survive verbatim because `LocatorExtent` names its own
`external_id_run` and its ordinals are run-local (this is what makes F1 a bug in the *base* locator
and not in the extents). `dict_extents`' positional rule survives a verbatim carry-forward provided
the hard links land at the same prefix-relative paths — worth one sentence in §3 pass 4, because a
flush-published extent's path is `…/segments/<seg_id>/terms-0.dict` and the new prefix has no such
segment, so the fold must either recreate that directory as a link target or rewrite the path
strings (legal: the list's *order* is positional, its paths are data). The reader protocol still
works: `honourability` is computed from `tombstones`/`deny`/`deltas` only, all honoured, so a
fold-published manifest is `Honourable` and step-down behaves. `permutation.bin`'s `bound` is defined
as the draft assumes (contracts §2.6: *"that partition-slice's max build entity + 1"*), and
`RowSpace::with_extent`'s `entity_lo >= base.bound()` check is satisfied by a fold bound at
max-folded-entity+1. Nothing the fold writes mutates a file: it writes a whole new prefix and flips
`CURRENT`, which is the sanctioned path.

**Task C — buildability.** `check-layers.sh` rule 1 (one publisher) is safe: the fold submits to the
executor, and the only `.store(`/`.swap(`/`.rcu(` outside `write.rs` would be the executor's own.
Rules 2–4 are untouched. See F7 for the edge the draft's "unaffected" line misses. Hard links are
available; the object-store answer §8 gives ("the link is a copy, and the fold's disc estimate has
to say which it is") is honest and correctly scoped. The engine can carry the live `Arc<Dict>`
across the fold, so §3 pass 4's claim that neither the 7.1 GB clone nor the 40–53 s lookup-map
rebuild recurs is right.

**Task D — internal and cross-document consistency.** Write-path §8's obligation list: Rule F's
three gaps, the evaluate-entry descriptor rule (dissolved by 0048, correctly), the post-snapshot
deletion, the immortal overlay, `overlay_soft_limit`'s missing lever, the dictionary's monotone
length, decision 0043's dedicated thread, the carried-forward suppression set, and I2's forward
obligation are each genuinely discharged in the body, not merely mentioned. The one that is not is
the never-reused `seg_id` namespace (F14). Rule S and Rule F are kept apart correctly throughout,
including in the §2 table and §12's obligations 1 and 4 — the conflation this corpus has caught
twice does not recur. §5's durability argument (retirement is durable only after a rotation whose
head snapshot postdates the fold; the pre-rotation restart resurrects harmlessly) matches
`rotate_if_grown` and the seed-before-replay order in `Engine::open`, and §12's obligation 15 asks
for exactly the right assertion.

**Incidental, outside the document.** `Engine::publish_geometry`'s doc still says *"`Engine`'s
`postings` reader, **`dict`** and the `FragmentCache`'s `bundle_identity` are all bound at
`Engine::open` for the process lifetime"* — `dict` is a parameter of that very function and is
swapped in the `Generation` literal. Stale since the flush widened the signature. The draft quotes
only the true parts, so this is not a finding against it. Separately, decision 0048 cites *"compaction
§12's D4"*; the draft's D4 is at spec §13 (§12 is the test list) — a cross-reference that drifted
inside one day.
