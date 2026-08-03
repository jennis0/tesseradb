# Suppression as a set, not a field — mechanism redesign proposal

**Status:** Proposal, 2026-08-03 — **adopted the same day** (`6491d19`), with both escalations
ruled as recommended: the behaviour change accepted with a test naming it, and the full three-store
split done in one change. Evidence memo, never normative; kept as the record of why the shape was
chosen and what was refuted. `concurrency-lifecycle.md` §3 still describes the old three-field
representation and is Task 24's to catch up.

## Recommendation

**Replace the overlay's per-entity struct with three independent stores, one per retirement
rule** — and in particular make suppression an entity-space Roaring bitmap:

```
deleted:    Bitmap                              — grows only; retires by the stamp ledger (⊘)
suppressed: Bitmap                              — add on suppress, remove on unsuppress; nothing else touches it
evaluate:   FxHashMap<EntityId, PredicateChange> — set by predicate; retires at the fold (⊘)
```

The subtraction happens **at composition time and nowhere else**: `compose` treats
`deleted ∪ suppressed` as its fail set exactly as `verdict` does today, and the frozen fragment,
the persisted fragment cache and the row-projection cache remain deny-free. The client contract
(`/control/changes` by external id, `unsuppress` with no argument), the WAL record types, and the
side-manifest `deny` shape all survive unchanged; the only on-disk delta is that the rotation
snapshot loses its neutral-entry rule (see below), which is a deletion, not a break.

This is the starting hypothesis's bitmap, but **not** its "term readers never hold": putting the
set in the term index (option c) is refuted below. The bitmap is right; the postings are the wrong
place for it.

**What it deletes** (all verified against the current tree):

- `OverlayEntry`, `is_neutral`, and the neutral-husk snapshot rule (`overlay.rs:155–186`) — the
  subtlest piece of rotation, existing only because `suppressed` shares a map with two facts that
  never clear, so `suppress → unsuppress` leaves a husk whose *presence* is load-bearing.
- The last-write-wins enum hazard, structurally: the fail-open caught twice in review required
  three facts sharing one slot. Three stores of three different types in three places cannot be
  collapsed by a refactor that still compiles.
- The r1 fail-open, unrepresentably: the suppression bitmap **carries no stamp field at all**, so
  "assign every deny a retirement stamp" — the rejected single rule — cannot even be written
  against it. Its only removal path is the `Unsuppress` apply.
- The overlay-walk-and-filter a manifest `deny` writer would otherwise need: the flush's `deny`
  field becomes "serialise the bitmap", O(suppressed) not O(overlay), and the suppression-count
  metric becomes `cardinality()`.
- The "nothing shrinks the overlay" caveat on the soft-limit alarm (`write.rs:338`): unsuppress
  genuinely shrinks the suppression set, so the gauge regains a downward direction for the one
  disposition that has one.

**What it costs:** `verdict` consults two bitmaps and a map instead of one map — still one
function, still the single source of `deleted > suppressed > evaluate > buffered`. `compose`'s
overlay loop becomes three short loops (or one loop over the union of keys). Per-request cost is
unchanged: O(|deleted| + |suppressed| + |evaluate|) `row_of` lookups, exactly today's
O(|overlay|) walk *(modelled from the code's shape; neither variant has been benchmarked —
overlay sizes in every fixture are far below where it could matter)*. `tessera-lifecycle` gains a
`croaring` dependency it does not have today (it already depends on `tessera-authz`, which does —
no layer edge moves).

**One behaviour change, flagged for ruling** (§Escalations): an item that is ingested,
suppressed, then unsuppressed **before its first flush** is invisible today (the husk outranks the
buffer) and becomes visible under this design (its own terms decide, per I1's `direct_eval`).
Lifecycle §3.1 says "unsuppress removes the entry"; the code keeps the husk. The redesign matches
the spec and the operator's intent; the current behaviour is an artefact the snapshot rule then
had to preserve.

## Comparison

| | (a) status quo field | (b) suppression bitmap | (c) unsatisfiable-term posting | (d) `predicate([])` | (e) three stores (recommended) |
|---|---|---|---|---|---|
| Lifecycle (WAL, rotation, retirement) | neutral-husk snapshot rule; enum-collapse hazard | husk rule deleted for suppression; husk still possible via `unsuppress` on untouched entity | needs a postings write path, anti-posting ordering, per-deny fragment invalidation | retirement rule conflated with the fold; unsuppress payload becomes authorisation | husk machinery deleted entirely; snapshot = three canonical blobs |
| Ingest / flush | untouched; manifest `deny` needs overlay walk | untouched; `deny` = serialise bitmap | flush must write deny postings; unsuppress cannot remove them | flush's evaluate rule now applies to suppressions | untouched; `deny` and `tombstones` both trivial |
| Read path | O(overlay) walk per compose | same cost; optional shared row-space cache | AND-NOT in the fragment build **and** compose (redundant) | evaluate probe, as today | same as (b) |
| Fail-open route | collapse-to-enum (caught twice); husk dropped at rotation | none found; stamp unrepresentable | **yes — cached persisted fragments predate a suppress** | **yes-adjacent — wrong unsuppress terms silently re-authorise** | none found; stamp unrepresentable |
| Format / contract break | — | none required | postings format, fragment cache key | client contract: unsuppress takes terms | none required (snapshot encoding may simplify; optional) |
| Verdict | baseline; workable but subtle | viable | **NOT viable** | **NOT viable** | **recommended** |

## The options, argued

### (a) The status quo, honestly costed

It works, it is tested, and `delete → suppress → unsuppress` is covered. Its real costs are not
performance but reviewability, concentrated exactly where the brief asks for simplicity:

- **The neutral husk.** `Overlay::apply(Unsuppress)` uses `or_default`, so unsuppress *creates or
  keeps* an entry with all facts inactive. Because `verdict` returns early on any entry, the husk
  outranks the buffer, and `Overlay::snapshot` must therefore emit a synthetic `Unsuppress` record
  per husk so rotation reproduces the behaviour — three paragraphs of module doc defending a state
  the spec ("unsuppress removes the entry", lifecycle §3.1) says should not exist. Every future
  reader of rotation must re-derive why.
- **The collapse temptation is permanent.** Three booleans in one struct is the shape a
  simplifying refactor unifies; it has been caught twice and the defence is doc and tests, not
  structure.
- **Suppression has set semantics wearing a map's costs**: the manifest `deny` writer (unbuilt —
  `execute_flush` clones the prior manifest and never sets `deny`; the only writer in the tree is
  `tessera-build/src/lib.rs:567`, `Vec::new()`), the suppression-count metric, and the immediate
  deny-publication path (contracts §2.3, also unbuilt) each need an O(overlay) walk with a filter.

### (b) / (e) The suppression bitmap, and the full split

(b) pulls only `suppressed` out; (e) splits all three. (e) is recommended because after (b) the
remaining `OverlayEntry` holds `deleted: bool` (never cleared) and `evaluate: Option<_>` (never
cleared) — a map to a struct of two facts neither of which ever retires, i.e. two more sets. The
step from (b) to (e) deletes `OverlayEntry` outright and costs one afternoon while formats are
free; done later it costs a migration.

**Read path.** `verdict` keeps its shape and its single-source status:

```
deleted.contains(e) || suppressed.contains(e)  → Some(false)
evaluate.get(e)                                → Some(terms ∩ satisfied ≠ ∅)
e ≥ watermark && buffer.get(e)                 → Some(terms ∩ satisfied ≠ ∅)
otherwise                                      → None (fragment decides)
```

`compose` iterates `deleted`, `suppressed`, `evaluate` keys and the buffer instead of one overlay
map; the `∩ base` / `∖ base` clamps and the `minus`/`plus` construction are untouched, so
`EffectiveMask` and everything above it do not change at all. An entity in several stores lands in
`minus` once — bitmap semantics deduplicate.

**Not adopted now, recorded so it is not re-derived:** a shared row-space projection of
`deleted ∪ suppressed`, keyed on `segments_version` (never the prefix — I11), maintained
incrementally on each accepted change, would turn compose's per-entity `row_of` loop into one
`AND` at O(containers). It is legal and it is a *fourth* cache with an invalidation surface;
nothing measured says the per-entity loop is a cost. Leave it until a measurement asks for it.

**Lifecycle.** Suppress/unsuppress stay `Change` records; replay applies them into the bitmap
through the same external-id resolution, and `initial_deny` seeding (`write.rs::reconstruct`)
inserts into the bitmap instead of the map — the seed-then-union argument survives verbatim
because bitmap insertion is idempotent and no manifest carries an `Unsuppress`. The rotation
snapshot emits the bitmap's members as `Suppress` entries (no new record type needed) or, if the
encoding is touched anyway, as one sorted-`u32` blob per store — sorted values, not serialised
Roaring, because `retry_durability`'s rewrite needs bytes that are a function of the state alone,
and Roaring's container choice is a function of history unless `run_optimize` is re-argued each
time. The determinism obligation transfers; the husk rule does not.

**Ingest and flush.** Nothing. `plan_flush` already consults only `deleted` (now a bitmap probe);
suppressed items already flush normally so unsuppress has a row to reveal; the buffer is
untouched. Flush's manifest gains `deny: serialise(suppressed)` and
`tombstones: serialise(deleted)` — the two unbuilt writers become one-liners, which is the point.

**Survival across the lifecycle events, checked:** restart — WAL replay plus manifest seeding, as
today. Rotation — snapshot fsynced before deletion, §7.2 unchanged, minus one rule. Merge —
permutes row space only; both bitmaps are entity-space and unaffected (the optional row-space
cache, if ever built, rotates with `segments_version`, which a merge bumps). Compaction —
§5.3's "active suppression set carried forward verbatim" becomes copying one bitmap.

**Why it structurally cannot fail open:** the suppression store has exactly two mutation sites
(insert on `Suppress`, remove on `Unsuppress`) and no field on which retirement machinery could
ever act — the stamp ledger and the fold operate on stamps the bitmap does not carry, so the r1
unification is not expressible, not merely forbidden. `delete → suppress → unsuppress` cannot
re-expose because unsuppress mutates a store that does not contain the deletion. The one rule
that must be kept by discipline: **the subtraction lives in `compose`/`verdict` only** — see (c)
for what goes wrong anywhere else.

### (c) A suppression posting under an unsatisfiable term — NOT viable

NOT viable, for a structural reason that also hardens the recommendation:

- **The persisted fragment cache fails open.** `FrozenFragment`s are built once and persisted to
  disk keyed on `(bundle_identity, auth_plugin_hash, terms, watermark)` — no deny component. A
  fragment with the AND-NOT baked in that was built *before* a suppression is served *after* it,
  and the suppressed item is inside it. Closing that means keying the cache on a suppression
  version, which forces every session to repay the fragment build (and the measured 10.7 s
  row projection at 10⁹) **per accepted deny, fleet-wide** — the exact cost the frozen-fragment
  design exists to pay once. Alternatively compose keeps subtracting as a backstop — at which
  point the fragment-build AND-NOT is redundant machinery in the most security-critical function
  in the system, and the honest move is to delete it. Fact 2 of the brief holds as stated:
  suppression state cannot live only in the fragment, and once it lives outside it, it need not
  live inside it at all.
- **Postings are append-only; unsuppress is a removal.** Base plus tiers can only add.
  Representing unsuppress needs anti-postings with an ordering rule between tiers — r1's
  stamp-ordering problem, rebuilt in the index.
- Fact 1 confirmed in code: `build_fragment_with_deltas` is `fast_or` over satisfied terms'
  postings; a term the reader never holds contributes nothing, so the mechanism only works with a
  new negative path through the fragment build — capability entering somewhere other than the
  filter contract (§8.2).

### (d) Collapse into `predicate([])` — NOT viable

Fact 5 is real — a descriptor-less `Predicate` already folds to an unsatisfiable set and hides
the item — but promoting it to *the* suppression mechanism fails twice:

- **Unsuppress becomes an authorisation write.** It takes no argument today; under (d) the client
  must supply the item's original terms back, and a wrong set silently *changes* what the item is,
  with no error and no symptom. An operator flow whose purpose is temporary hiding acquires the
  power to re-author predicates by typo. The corpus's own line — pins fix geometry, never
  authorisation — is the same separation from the other side: suppression must not be an
  authorisation editor.
- **The retirement rule conflates.** Evaluate entries retire at the compaction fold; the fold
  writes the (empty) term set into base postings and drops the entry. The item stays hidden —
  fail-closed — but the postings that carried its real terms are now gone, so a post-fold
  unsuppress cannot restore visibility from anything the *server* holds. Suppression's "retires
  only on unsuppress" and evaluate's "retires at the fold" are different rules because they are
  different relationships to postings (lifecycle §3.1's middle column); (d) is precisely the
  conflation the three-rule split exists to forbid, arriving via the third rule instead of the
  first.

## Escalations — rulable as stated

1. **Unsuppress on a still-buffered item.** Today: stays invisible until the next flush (an
   overlay husk outranks the buffer). Under the redesign: visible again immediately, its own
   terms deciding, exactly as a never-suppressed buffered item is. The window is one flush tick.
   Options: (i) accept the new behaviour — it is what lifecycle §3.1's "unsuppress removes the
   entry" already says, and it is fail-closed in the only direction that matters (nothing is shown
   that `direct_eval` refuses); (ii) preserve today's behaviour, which requires reintroducing a
   husk set whose only purpose is to reproduce an artefact. **Recommended: (i).**
2. **Scope of the change.** (b) suppression-only, or (e) the full three-store split in one
   change. (e) deletes `OverlayEntry` and both unbuilt manifest writers become trivial; the extra
   surface over (b) is the `deleted` bitmap and the `evaluate` map extraction, neither of which
   changes any answer. **Recommended: (e), now, while formats are free.** If deferred, note that
   the future stamp ledger fits the bitmap shape *better* than the struct: deletions grouped as
   `BTreeMap<stamp, Bitmap>` retire by dropping leading entries, which is exactly §3.2's
   "retire the prefix below `min_live_stamp`" — recorded here so the ledger stage does not
   re-derive it, and explicitly not designed here.

## Evidence discipline

- **Measured** (elsewhere, cited): 10.7 s row projection at 10⁹; deny ack 3.2 ms quiescent /
  165 ms p50 under 1 M buffered ingest; bitmap ops O(containers touched). Nothing in this memo
  was newly measured.
- **Modelled**: compose cost equality between the overlay walk and the three-store walk (same
  asymptotics, same `row_of` per entity); the fleet-wide rebuild cost under (c)'s
  suppression-versioned cache key.
- **Assumed**: suppression sets stay small enough that cloning the bitmap per accepted change
  (the clone-and-swap generation discipline) is negligible — true at any size the overlay soft
  limit tolerates today, since a bitmap clone is strictly cheaper than the `FxHashMap` clone the
  acceptance path performs now.

## Brief-claim verification

All nine established facts were checked against the tree; eight hold as stated. Fact 6/7's code
sites: `overlay.rs` (three fields, `apply`), `compose.rs::verdict` (single-sourced precedence),
`flush.rs` (per-disposition flush rules). Fact 8 confirmed: `SegmentsManifest::deny`'s one
non-test writer is the build, writing empty; `execute_flush` inherits the prior manifest's empty
`deny`, so the flush-published manifest silently claims no suppressions — the unbuilt writer this
memo's recommendation reduces to a serialisation call. The one divergence found: lifecycle §3.1's
"unsuppress removes the entry" is not what the code does (the husk survives), which is escalation
1 rather than a fact to lean on.
