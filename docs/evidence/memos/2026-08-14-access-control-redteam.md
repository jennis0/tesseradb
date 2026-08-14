# Access-control red-team — can a legitimate viewer read what it was not granted?

**Status:** Evidence — red-team transcript, never normative. A live-server assessment of the
serving API against one question: can a principal, holding whatever grant it likes, obtain a point
or an indexed/loaded value outside its own `M_auth`? Two passes — the request contract, then the
memory and data-structure surface beneath it. **Result: no bypass found.** The attacks that failed
are kept in full so they are not re-run; the latent hazards and recommendations are the part worth
acting on.

## Scope

- **Surface:** the viewer plane (`/v1/meta`, `/v1/viewport`, `/v1/items`, `/v1/categories`), the
  session plane, and the control plane treated as a *data source* — a legitimate operator
  ingesting, deleting, suppressing and folding while the attacker reads.
- **Excluded, by the brief:** the authentication mechanism itself (the attacker may *set* its grant
  to any term combination it likes, but not forge a credential); and timing/existence channels that
  reveal only that other data exists without its content — Appendix C already accepts two of these
  (C4, the drill-down/stage-timing channel; C24, the `public`-category scan-cost channel).
- **Rig:** the release binary over the adversarial mask catalogue (`reference/oracle/catalogue.py`,
  150 000 items, one partition, one slice, `builtin:passthrough`), driven over HTTP. Several
  contrived principals — seeing 15 items (`sparse`), 100 (`boundary`), 3 750 (`cross_lo`), all, and
  none — with the all-seeing principal used only to learn the ground truth then attacked as a narrow
  one. Every reported figure is **measured** on this rig unless marked otherwise.

## Result

The boundary held on every surface, and it holds *structurally* rather than by discipline: a
viewer's visible set is materialised once as a Roaring fragment at authorise (I2), and every count,
sample, filter result, density cell, category value and drill-down is arithmetic over that one set.
There is no route that derives a quantity from the whole corpus and then gates it, which is the
shape Appendix C's prior-art survey catalogues as the leak — so there is nothing to trick into
over-disclosing.

For each narrow principal, every number the engine returned was compared against a ground truth
computed **only from the entities that principal may see** (the fixture's own generation functions,
never the served `attrs/` artefact). They agreed everywhere.

## Pass 1 — the request contract

Each row is the mechanism, the property it must hold, and the measured observation.

- **Drill-down, cross-principal** (`POST /v1/items`). Took a hidden item's `tessera_id` from the
  all-seeing principal and presented it as a narrow one; drill-down returns the full record, blob
  text included, so a hole here is a total content leak. → hidden id `404 unknown`; visible id
  `200` with fields; the two `None`-shaped cases (no such id / exists-but-invisible) are one code
  path (contracts §3.2, C4).
- **Viewport counts** (`visible`/`matched`/`served`). `matched ≤ visible` on every tile; served
  points a subset of the visible set; `sum(served) = points returned`. Holds unfiltered and under
  every filter.
- **Filters, four families** (numeric, category, keyword, text). `matched` equalled the brute-force
  count over *visible entities only* for every probe; a needle only a hidden item carries — a
  category code, a keyword, a unique abstract token, a phrase — returned zero. The text `phrase`
  route decompresses the record blob at query time (the one filter route that reads a stored value;
  records §4.4) and still under-reads to the composed candidate: `phrase "harbour ref1065601"`, a
  bigram only a hidden item's prose holds, matched 0 for a principal that cannot see it.
- **Boolean composition** (`all_of`/`any_of`/`none_of`). Every composition narrowed, never widened;
  `none_of` subtracted within the mask (filter-surface §5.1) and is correctly refused over a `text`
  column, which stores no per-item value to negate (`FilterError::NegationWithoutPresence`, 422).
- **Category enumeration** (`/v1/categories/{column}`). A `per_viewer` column (C11,
  per-point-attributes §3.3) offered only values carried by a visible item — the `sparse` principal
  saw `{alpha, beta, gamma}`, never `omega`/`solo`, which only hidden entities carry — and the two
  request doors (page, resolve-codes) share one gate. A `public` column is schema and served to all
  by design (decision 0063, C24).
- **Density underlay** (`underlay_offset`). Exact per-cell counts summed to the *masked* total
  (3 749), not the corpus total; the empty principal's underlay summed to 0 (I2).
- **Deny lifecycle** (`/control/changes`). `delete` → hidden; `suppress`→`unsuppress` → reappears
  (Rule S); `delete`→`suppress`→`unsuppress` → **stays hidden** — the single most-warned-about
  fail-open in the tree (`compose::derive_denied`: an unsuppress must not subtract a row while
  `deleted` still holds it). `apply_changes` re-derives the deny mask in full on any window carrying
  an unsuppress and only ever *adds* rows otherwise, so even in release (where the equality
  `debug_assert` is compiled out) the fold direction is fail-closed.
- **Delta-tier fragment build** (`/control/ingest` + flush). A flush publishes one per-term posting
  tier; the union must be over the granted terms only, never a tier wholesale (I2,
  `build_fragment_with_deltas`). Ingested a term-7 item and a term-4 item: the holder of term 4 saw
  the term-4 item and never the term-7 one, and vice-versa; a session authorised *before* the flush
  picked up the term-4 item via the background refresh and never the term-7 one.
- **Grant-combination monotonicity** (the brief's own example). Visibility is exactly the union of
  the granted terms' postings — strictly monotone — so holding X+Y can never unlock something
  requiring Z. There is no AND / required-set logic in the single-partition build for a combination
  to exploit (that logic is I13's compartmented gating, ⊘ unbuilt — see below).
- **Identifier enumeration** (`tessera_id`). A spread of raw `u64`s (0, 1, 2⁶³, 2⁶⁴−1, …) all
  returned `404`; the served `tessera_id` is not the entity id (decision 0014, I10).
- **Credential / plane separation.** Viewer, session and control are distinct listeners; control
  routes are absent from the viewer socket; the shared-secret compare is hash-then-constant-time
  XOR, with no prefix oracle (`AppState::check_bearer`).
- **Malformed input.** Unknown column `422`, unknown slice `404`, non-filterable column `422`,
  `bbox`+`tiles` together `422`, stale `idset` `409`, the absent sentinel code 0 omitted rather
  than served.
- **Compaction fold** (`POST /control/compact`, Rule F). The fold rewrites the corpus and retires
  executed deletions; a deleted item stayed hidden across the fold and the overlay drained to zero —
  no resurrection.

## Pass 2 — memory and data-structure surface

The second pass asks where a value the attacker controls becomes an **index or a length** into a
data structure or an `unsafe` view, and whether a slip there could read adjacent memory into a
response — a leak the logical checks never see, because they run on whatever was read.

**The `unsafe` inventory is confined to opening trusted artefacts, not indexing them with request
input.** Almost every `unsafe` block in the serving crates is one pattern — `mmap` a file and wrap
it as an Arrow buffer via `Buffer::from_custom_allocation` — plus the SHA-256-verified `Frozen`
bitmap views for the mask itself (`fragment.rs`) and the base postings (`postings.rs`). Each reads
**trusted on-disk data** whose length is checked once at bundle open (`permutation::validate_rows`,
`PostingsReader::open`, `Dict` open), never recomputed from a request. The `permutation.bin` slice
cast is guarded by an alignment argument and a `bound * 4` length checked at load.

**Every request field that becomes an index is bounds-checked or resolved through a keyed lookup:**

- **Category code** — a raw integer in a filter is accepted as any `u32` (`filter_dto::category_value`)
  and flows to `ColumnPostings::entities`/`posting_at`, which resolves it through a *keyed* tier and
  bounds-checks the ordinal (`idx >= len → None`). Codes 7, 255, 65 536 and 2³²−1 all returned
  `200` with `matched = 0` — a keyed miss, not an out-of-bounds read. `column.rs`'s own tests
  already cover sparse codes (40 000, 65 535).
- **Permutation** (`row_of`, entity→row) — guarded by `raw >= bound`, and `validate_rows` proves at
  open that the map is a bijection into `[0, row_count)`, so a `RowId` can never index
  `columns.arrow` out of range (I4/I11).
- **Numeric bound** — an `i128` from JSON, compared and never used as an offset; values beyond
  `u64`, negative, and inverted ranges resolved to a clean empty match with no overflow.
- **Tile prefix / zoom / underlay offset** — each validated against the grid before deriving a row
  range: a prefix with bits above the depth is `422`, `zoom > 16` is `422`, an over-budget tile
  count is `422`, `underlay_offset` over the configured maximum is `422` — refused, never clamped.
- **Filter nesting** — depth 41 refused at the `MAX_FILTER_DEPTH` guard (`422`).

**The `tessera_id` path — one entity, checked then read.** This is the richest handle: a `u64` the
attacker fully controls, inverted to an entity and then used to read a whole record. The inversion
is a balanced Feistel network (decision 0014, `IdentityKey::invert`), a true bijection, so *any*
`u64` inverts to *some* `(shard, entity)` — which forces the caller to validate everything, and
`Engine::item` does: **idset → shard → visibility** all gate before a byte is located, and the row
lookup, the entity-space value reads and the blob read all key on the *same* inverted entity, so
check-entity and read-entity cannot diverge. The blob read bounds-checks its field tag against the
schema and fails closed on any addressing defect rather than serving a neighbour's field under this
item's identity (records §3, review B6). The multi-segment resolution
(`segments.iter().rev().find(base)`) is the one the doc warns about taking wrong, and it is taken
right.

**Drill-down value integrity — the surface conformance defers.** The suite checks the record blob's
*addressing* self-consistency but explicitly defers drill-down *value* equality
(`oracle.catalogue.record_of`, "the waiting expectation"). Tested directly: for 333 entities —
every block edge, both container boundaries (65 536, 131 072), the planted oversize-blob row, the
empty-string row, the present-zero `pages`, first and last — `GET /v1/items` returned each entity's
own planted values across all three homes, **zero mismatches**. At scale, per restricted principal,
the served point set equalled the visible entity set exactly: `sparse` 15/15, `cross_lo` 3 750/3 750,
`boundary` 100/100, with **zero** points served outside the mask and none missing.

**Structural fuzz** at every index (extreme category codes, `i128` ranges past `u64`, depth-16
tiles with extreme prefixes, weird keyword needles including NUL bytes and 5 000-character strings,
nesting depth 41) produced no `500`-carrying-data, no wrong content, and no panic — guards fire,
keyed misses stay empty.

## Latent hazards — safe today only because something else is absent

None of these leaks now; each is safe because a neighbouring thing does not yet exist, so each is
where the next regression lands.

- **The row-projection cache key rests on an un-enforced invariant.** `RowProjectionKey` is keyed on
  `token_id`, safe only because it is drawn from a per-process monotonic counter that never repeats
  and dies with the process — the type cannot enforce it, and `cache.rs` says so in prose. A cold
  miss is a *measured* ~10.7 s at 10⁹, which makes persisting the cache an obvious future
  optimisation; the day it is persisted without widening the key, a reused `token_id` serves one
  principal's projection to another — cross-principal mask reuse (I2/I3). One edit from a leak,
  guarded only by a comment.
- **⊘ Partition fail-closed (I13b/c) is unbuilt.** The rule that an unconsulted compartment fails
  closed, and an unreachable one errors rather than contributing an empty set, has no
  implementation and no test; it is safe purely because there is one partition. It becomes live the
  day compartmented multi-partition MAC lands, and a missing required-set gate is the fail-*open*
  direction — this is also the only place a grant *combination* could ever produce emergent access,
  so pass 1's monotonicity result is contingent on it.
- **⊘ The disclosure floor is parsed but never enforced.** `min_visible_members` (§7.5/§2.3) is
  validated at startup and read by no handler (`AppState::min_visible_members` is `dead_code`), so a
  viewport or underlay cell can report an exact count of 1. This is *within* the caller's own
  `M_auth` — not a cross-principal leak, and outside the brief's target — but it is a specified
  k-anonymity-style control that is currently inert.
- **Documentation drift on compaction.** `docs/design/README.md`'s "Specified, not implemented"
  section still lists "there is no compaction, so [deletions] never retire" among the gaps that
  matter, and reasons "safe today only because nothing retires at all". Compaction **is** built
  (compaction.md is normative; CLAUDE.md's non-negotiables say the fold "exists, retires, and is
  scheduled"), and this assessment drove `POST /control/compact` and watched Rule F retire the
  deletions correctly. The stale bullet is harmless in itself, but it is exactly the "safe because
  absent" reasoning a future reviewer leans on when the operative reasoning has changed to "safe
  because the fold retires correctly".

## Recommendations

1. **Turn the cache-key invariant into a guard, not a comment** (highest severity averted). A
   `debug_assert`, or a newtype that stamps process identity into `RowProjectionKey`, so that
   persisting the projection cache or reusing a `token_id` fails loudly rather than serving a
   cross-principal mask silently.
2. **Land I13b/c with — not after — multi-partition.** The required-set gate and the
   unconsulted-partition-fails-closed rule must ship in the same change that first produces more
   than one partition; a silently skipped partition is the fail-open case and nothing tests it
   today.
3. **Decide the disclosure floor's fate explicitly.** Either wire `min_visible_members` into the
   count/underlay path, or annotate it at the config site as a deferred control, so its inertness
   is a recorded decision rather than a surprise for whoever assumes it is enforced.
4. **Reconcile the compaction docs.** Update `README.md`'s "Specified, not implemented" section so
   the built-and-working fold is not still described as absent.
5. **Extend the visible-only differential to the plugin boundary.** Every test here ran against
   `builtin:passthrough`, where a grant is a literal term list. A real deployment's authorisation
   logic lives in its plugin's `terms_of_label` and `auth_data` parse — where an access-control bug
   would actually be written — and that surface deserves the same differential applied to the
   engine here.
6. **Close the memory-safety gap with tooling.** The `unsafe` surface held up to source review and
   black-box probing, but the higher-confidence guarantee is a fuzzer against the Arrow-ingest and
   wire decoders plus a Miri/ASan run of the suite, so a slip in an index path becomes a test
   failure rather than the next assessment's finding.

## What this did not cover

- **One synthetic corpus**, one partition, one slice; multi-partition compartmenting and
  multi-slice addressing were not exercised because the build does not yet produce them.
- **`builtin:passthrough` only** (recommendation 5).
- **Memory safety was reasoned from source over the `unsafe` inventory and probed from outside**;
  it is not a sanitiser or fuzzer run (recommendation 6).
- **I10 is confirmed at the served surface** (a `tessera_id` is not an entity id) but a byte-pattern
  scan is necessary-not-sufficient; the structural half is the wire crate's handle mint, reviewed
  in `test_byte_scan.py`'s companion argument, not re-derived here.
