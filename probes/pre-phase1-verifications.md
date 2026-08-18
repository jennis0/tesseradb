# Pre-Phase-1 verifications — 2026-07-27

The architecture doc (r4 §12) and plan §3 list three things to verify
before Phase 1 commits. All three checked; **no blockers**. Sources are
docs.rs source views for `croaring` 2.7.0 (updated 2026-06-25, 1.55M
downloads) and the PyPI/crates.io APIs.

## 1. `croaring` frozen views — CONFIRMED

The plan's flagged risk ("if that binding has regressed, the fallback is
a thin FFI shim") does not apply. In `croaring` 2.7.0, `Frozen` is a
first-class format implementing **both** halves:

```rust
impl Serializer for Frozen {
    const REQUIRED_ALIGNMENT: usize = 32;
    fn get_serialized_size_in_bytes(..) -> usize   // roaring_bitmap_frozen_size_in_bytes
    unsafe fn raw_serialize(..)                    // roaring_bitmap_frozen_serialize
}
impl ViewDeserializer for Frozen {
    unsafe fn deserialize_view(data: &[u8]) -> BitmapView<'_>  // roaring_bitmap_frozen_view
}
```

Safety contract, which becomes a **bundle-format requirement**: the
frozen buffer must start **32-byte aligned** and `data.len()` must equal
the frozen size exactly. So mask files need 32-byte-aligned offsets
(and mmap base alignment is already page-granular) — a §4.1 bundle
detail worth stating explicitly rather than discovering.

Caveat on docs: `Bitmap::serialize()` carries the note "cannot be used
with formats that require alignment, such as `Frozen`", and
`BitmapView::deserialize` is the safe/no-align path. Reading only those
suggests frozen views are absent; the `deserialize_view` impl above is
the real answer. (A first pass here misread exactly that — recorded so
nobody re-derives the wrong conclusion.)

Primitive coverage on `Bitmap` (u32), all present: `range_cardinality`,
`and_cardinality`, `rank`, `select`, `intersect_with_range`,
`run_optimize`, `add_many`, `or_many` (bulk union — the authorise path),
`to_vec` (= `roaring_bitmap_to_uint32_array`).

**One gap, minor:** no binding for `roaring_bitmap_range_uint32_array`
(design §10.4 names it for writing a tile's visible IDs straight into a
gather buffer). Workarounds, in order of preference: `and` with a range
bitmap then `to_vec`; iterate the view; or a two-line FFI shim (the
symbol is in the linked C library). Not a redesign — record it as a
Phase 2 detail.

**Bitmap64 (u64) is thinner:** `range_cardinality`, `rank`, `select`,
`intersect_with_range` and a `Bitmap64View` exist, but **no `Frozen`
support and no `or_many`**. Relevant because entity space is `EntityId(u64)`
(architecture §3). Mitigations, none blocking: masks are cached in
*row* space (u32) for serving; entity-space fragments can stay 32-bit
per shard/partition under the scaling analysis's "per-shard row IDs in
u32, entity IDs u64 globally" split; or use `Treemap`. Decide when the
u32 entity ceiling is actually approached (§16 open question) — but
note the frozen-mmap story is **u32-only today**, which is an argument
for keeping cached fragments 32-bit.

## 2. PyPI name `tessera` — TAKEN, fallback needed

`tessera` 0.10.0 exists: "A dashboard front end for Graphite", last
released **2017-02-03**, Development Status 3-Alpha, no activity since.
Abandoned but occupying the name. PEP 541 transfer is slow and
uncertain, so a distinct distribution name was chosen instead.

**DECIDED: the distribution is `tessera-index`** (verified free
2026-07-27). The *import* name stays `tessera` and the CLI binary stays
`tessera` — only `pip install tessera-index` differs. Rejected:
`tessera-engine` (collides with the internal composition-root crate,
§3), `tessera-query` (overclaims a general query surface that Appendix H
explicitly refuses), `tessera-stream` (misdescribes the system —
streaming ingest exists, but this is not a stream processor),
`tessera-core` and `tesserae` (taken on PyPI).

*What it is an index for*, since the name now asserts it: an index over
the caller's corpus, **keyed on (viewer, region)**, answering counts,
densities, samples and label-visibility decisions. Not an index of
documents — the corpus stays with the caller and every artifact is
derived (§14). The composite key is the point: single-axis permission
indexes and single-axis spatial indexes are both textbook; indexing
their *intersection* so masked cardinality costs O(containers) is what
the survey found nowhere. The word carries three lineages the design
already claims — Leopard-style accessible-set materialisation, boolean
expression indexing, and index-sort-plus-skip-index — composed.

### The framing line (decided 2026-07-27)

> **A scalable, interactive backend for ordered 1D and 2D data, with
> per-item access control applied to counts, densities, hierarchies and
> labels — not just to retrieval.**

Use this as the project's one-line description: distribution summary,
repository blurb, the sentence at the top of a deck. Why each part:

- *ordered 1D and 2D data* — names the actual requirement (a total order
  in which a query region is a small number of contiguous ranges and the
  hierarchy nests) rather than a domain. Honest caveat: 1D is the
  machinery's shape, not a shipped axis — valid-time is the uncommitted
  Appendix F, and §9's temporal views are discrete partitions, not an
  ordered axis. This is the clause someone will eventually hold us to.
- *per-item access control* — the established phrasing, and more precise
  than "per-viewer": predicates live on items, masks are per viewer.
- *counts, densities, hierarchies and labels* — **enumerated, not
  generalised.** "…and other aggregations" was proposed and rejected:
  Appendix H draws a hard line ("a counting engine, not an aggregation
  engine" — sums, means and percentiles are O(visible items) and out of
  scope), and the enumeration is itself the safety property, since the
  leak register is exhaustive only while the surface is ~five shapes.
- *not just to retrieval* — the differentiator, legible to anyone who
  knows the field: drawing the line at retrieval is exactly what every
  surveyed system does.

Rejected framings, recorded because they will be re-proposed:
*"projected document corpora"* — too narrow (items need not be
documents; the machinery does not require a projection);
*"spatio-temporal"* — overstates (valid time is uncommitted; "spatial"
imports geographic semantics the design disowns) while simultaneously
understating (nothing in the machinery is two-dimensional — Appendix H's
faceted counts need no ordering at all); *mechanism-first phrasings*
("every count computed only from the viewer's visible set") — accurate
but parse only for someone who already knows the system; they belong in
the long description, not the summary. Appendix H's own *materialised
per-viewer selection* remains the accurate general statement, and is
deliberately framing rather than scope.

**Reserve it with a placeholder release at Phase 1 kickoff** (the
availability check is a snapshot, not a hold), and check the crates.io
namespace for `tessera*` separately — the Rust crates publish under
their own names.

## 3. maturin binary-shipping — AVAILABLE

`maturin` 1.14.1 current. The pattern the architecture doc relies on
(ship a Rust binary inside the wheel, Python as supervisor — the `ruff`
precedent) is supported; no version risk found. Not exercised end to
end here — that is a Phase 1 packaging task, not a feasibility question.

## Actions

- Bundle format: state the 32-byte alignment requirement for frozen
  mask buffers in the architecture doc's §4.1 layout.
- Phase 2: note the `range_uint32_array` binding gap with its workaround.
- Phase 1 kickoff: pick the PyPI distribution name.
- §16 bookkeeping: entity-ID width interacts with frozen-view support
  (u32 only) — a new input to the exhaustion question.
