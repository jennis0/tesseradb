# Discovered vocabularies and code minting at ingest — design memo

**Status:** Design memo, 2026-08-07. Non-normative as evidence. Designs the closure of
`schema.toml`'s `vocabulary = "discovered"` refusal (per-point-attributes §3.4, §3.8 — cited below
as **attributes §n**) against the write path as built on `attrs/render-columns` (commit 1fc47ad).
Nothing here is implemented; every mechanism this memo proposes is ⊘ until built. Reads against:
attributes §3.1–§3.6, §3.8, §4.4, §5; write-path §1.2, §2.3, §4.3–§4.5, §5.4; contracts §2.2–§2.4,
§3.4; slices §61, §80, §87; the 2026-07-29 secondary-attribute-indexing memo §6.1–§6.2; decisions
0013, 0042, 0047, 0048, 0050; and the code in `tessera-authz`, `tessera-lifecycle`,
`tessera-engine`, `tessera-server`, `tessera-store` and `tessera-build` named per claim.

---

## 0. The result first

**A minted category code is not a dictionary ordinal, and nothing that assigns positions is reused
to assign codes.** The auth dictionary's whole correctness argument — `load(a ++ b) ≡
load(a).load_extending(b)`, extents positional and append-only, the moved-under discard — exists
*because* an ordinal is a position in a concatenation. Attributes §3.4 requires the opposite
object: a code drawn at random from the declared width's unused space, recorded beside its key,
pinned forever. The two id spaces coexist and are joined only through the key, exactly as
`entity_id` (dense, internal) and `tessera_id` (scattered, shown) already coexist joined through
the row. Once that is accepted, most of the apparent design collapses:

1. **Keys arrive on the wire; the server mints.** A category column in an ingest batch is `utf8`
   value keys, not codes at the declared width. Attributes §5's first paragraph is wrong for
   category columns and should be amended (§2 resolves the inconsistency that section carries).
2. **Minting happens on the write executor at the commit-window close**, beside entity allocation:
   allocate (entities *and* codes) → append mint records → append batch records → one fsync →
   apply → swap → ack. Serial by construction, so two windows cannot disagree (§4).
3. **The assignment becomes durable in a new `WalRecord::VocabularyMint` record**, appended in the
   same fsync as the batch that caused it; the row's `WalScalar` carries the code, resolved once at
   the close and persisted — the `ChangeByEntity` precedent, not the `WalRow.descriptors` one,
   because a code (unlike a term ordinal) is durable and bundle-independent from the moment it is
   minted (§3.1).
4. **The durable home between builds is a `vocabulary_extensions` field in `SEGMENTS-<n>.json`,
   restated in full at every manifest write** — the deny-fields precedent, not the `dict_extents`
   one. No extent files, no positional list, no coalesce, no moved-under guard. The compaction fold
   folds it verbatim into the new `MANIFEST.vocabularies` and empties it (§3.2, §3.3).
5. **Declare-then-use for `vocabulary = "declared"`**: an unknown key is a 422 naming the column
   and the key, whole batch without effect, checked in the ingest handler against the manifest's
   compiled vocabulary (§2.3).
6. **The `Dict`/postings/tier machinery is reused for nothing in this design.** It is reserved,
   untouched, for the attribute-postings namespace the `filter` placement will need (attributes
   §3.5), which stays ⊘ and out of scope (§5, §6).

What is genuinely new is small: a mint routine shared by build and ingest, a per-vocabulary live
view on the `Generation`, one WAL variant, one manifest field with its loader and fold step, and
the ingest-side key translation. Everything else is a parameter change to machinery that exists.

---

## 1. Two id spaces, joined through the key — the resolution of the allocation clash

The clash: `DictWriter` assigns dense ordinals in intern order; attributes §3.4 requires scattered
codes from the unused space. These do not compose, and they do not need to, because they describe
two different objects serving two different consumers:

| Object | Assignment | Consumer | Where stored |
|---|---|---|---|
| **Code** | random draw from the declared width's unused space, at mint, recorded | the row, the wire, the legend — everything a viewer can see | `columns.arrow` tail, `WalScalar`, `MANIFEST.vocabularies`, `SEGMENTS.vocabulary_extensions` |
| **Attribute term ordinal** (⊘ unbuilt) | dense, intern order, positional in extents | the attribute postings file the `filter` placement will read | the future attribute dictionary and postings namespace (attributes §3.5) — never a row, never the wire |

The disclosure argument of attributes §3.4 is about *visible* codes: dense codes make a visible
code a lower bound on vocabulary cardinality. The dictionary's ordinals are dense **and never
visible** — they index a postings file inside the trust boundary, precisely as entity ids index
masks inside the trust boundary while I10 keeps them off the wire. The system already runs this
exact split once: `entity_id` dense internally, `tessera_id` scattered externally, stored at the
row it is shown from (contracts §2.6 r6 — the sentence attributes §3.4 itself cites). Codes are
the `tessera_id` of vocabulary space; ordinals are its `entity_id`.

So a minted category value is **a key with a code recorded beside it in the vocabulary table**,
and nothing else. When the `filter` placement is eventually built, the attribute dictionary will
intern `(column, key)` descriptors (attributes §3.2's identity) and assign its own dense TermIds;
those TermIds and the codes are joined through the key by whatever builds the postings, and are
never converted numerically. No table maps code → ordinal; nothing needs one.

Two scoping rules that follow from the built manifest shape, stated so they are not blurred:

- **Minting is vocabulary-scoped, not column-scoped.** `ManifestVocabulary` is a named object and
  several columns may share it (`values_of`, attributes §3.9); codes are vocabulary-scoped so
  cross-slice and cross-column legends compose. Two columns sharing a discovered vocabulary mint
  into one code space through one view.
- **Visibility stays per-column** (attributes §3.2) — but visibility is membership-derived and
  membership is postings, which are ⊘. Nothing in this design computes visibility; it only mints
  and records. The per-column predicate binds the future postings work, not this one.

---

## 2. The wire form, and the inconsistency in attributes §5

### 2.1 The inconsistency, named

Attributes §5 says three things that cannot all hold:

- first paragraph: the existing scalar-tail validation "extends to attributes unchanged" — which,
  as built (`parse_ingest_batch` in `crates/tessera-server/src/control.rs`, matching the batch
  column's arrow type against `DeclaredScalar.arrow_type`), means category values arrive as
  **codes at the declared width**;
- last paragraph: an ingest row "naming an undeclared value" under `vocabulary = "declared"` is a
  422 — a rule about **keys**, unenforceable over bare integers;
- §3.6: code-space exhaustion is a 422 **at ingest** — which presupposes that ingest allocates.

The built behaviour today is the first reading, and it has a real hole the second paragraph was
written to close: **nothing validates a category code against its vocabulary.** A declared `u16`
`department` column accepts any `u16` — an unassigned code, a reserved code, a typo — and the row
stores it with no error anywhere. That is exactly the "typo must not create a category" failure
slices §80 rules out for slices, minus even the creation: the row carries a code no key explains.

### 2.2 The resolution: keys arrive, the server owns codes

**Category columns arrive as `utf8` keys. The caller never supplies a code, for any vocabulary
kind.** Three arguments, any one sufficient:

- Attributes §3.1's table already rules it: the code is *derived*, the key is *supplied*. A caller
  supplying codes for a discovered vocabulary is the minting authority, and the server can then
  guarantee neither scatter nor never-reuse — the two properties §3.4 exists for.
- Declare-then-use (§2.3 below) needs the key. A code can only be range-checked; a key can be
  membership-checked, and the membership check is the rule.
- §3.6's exhaustion-at-ingest is only reachable if ingest mints.

The declared width remains the **storage** type: the row, the WAL scalar, the segment column and
the viewer wire all carry the code at `DeclaredScalar.arrow_type`, unchanged from what 1fc47ad
built. Only the ingest wire changes. This is a breaking change to the admin plane relative to the
branch's own just-landed behaviour (the `build_attributes.py` fixture and the ingest tests carry
code-typed category columns); decision 0048 licenses it — no deployments exist.

### 2.3 What this does to `parse_ingest_batch`'s positional-safety argument

The current argument: every declared column present, no undeclared column, each column at the
declared arrow type, the scalar vector built in declared order — so a positional read-back cannot
misalign. The type equality `batch column type == d.arrow_type` is the piece that breaks: a
category's wire type (`utf8`) now differs from its storage type (`u8`/`u16`/`u32`).

The repair is to make the *expected wire type* a declaration-derived function rather than a field
read: `utf8` when `d.vocabulary.is_some()`, else `d.arrow_type`. The argument's structure is
untouched — it never rested on wire equalling storage, only on "each column at its **expected**
type, where expected is a function of the manifest declaration alone". The function must live in
**one place on `DeclaredScalar`** (`tessera-store/src/manifest.rs`), reached by the server through
the engine's re-export — the same one-parser rule 1fc47ad's commit message records learning when
two spellings of the type table (`u64` against `uint64`) disagreed. A second transcription of the
expected-type rule in `control.rs` is how a category column and a plain scalar column of the same
width come to be confused, and confusion here is a positional shift wearing a 200.

A side benefit worth keeping: a plain `u16` scalar and a `u16` category are now *distinguishable
on the wire* (integer against string), so a schema/client disagreement about whether a column is a
category surfaces as a 422 naming the column rather than as plausible integers stored as codes.

Rules at the column level, each a 422 naming the column (whole batch without effect, as every
tail refusal already is):

- **`vocabulary = "declared"`, unknown key** → 422 naming column and key (declare-then-use,
  attributes §5, slices §80). A retired key is an unknown key — retirement removes it from
  `values` and moves its code to `reserved` — so no separate rule is needed. Checked **in the
  ingest handler**, beside the existing whole-batch schema validation: a declared vocabulary is
  immutable between builds (property upserts touch labels, never keys or codes), so the handler's
  manifest snapshot cannot be stale, and the refusal costs nothing on the executor. Known keys are
  translated to codes in the handler for the same reason.
- **`vocabulary = "discovered"`, any key** → translated where known; unknown keys travel to the
  executor as keys and are minted at the window close (§4). The 422s here are the empty-string
  key (a typo trap, refused — an empty key is not a value and must not become one) and exhaustion
  (§3.6's typed error naming the column and its width, raised by the mint routine).
- **Absent value** → the reserved code 0. The wire spelling of absence needs an owner ruling
  (§9): a null in a nullable `utf8` column is the natural form, but the current tail is built on
  non-nullable columns and `scalar_of` never checks validity, so accepting nulls is new machinery
  either way.

---

## 3. Durability: where the assignment becomes real

### 3.1 The WAL record — why the row may carry the code where it may not carry a TermId

`WalRow.descriptors` deliberately carries raw descriptor bytes, never `TermId`s, because "term IDs
are bundle-relative ordinals fixed by the bundle's (immutable) dictionary extents, so a term
coined between builds has no durable ID yet" (`crates/tessera-lifecycle/src/wal.rs`). The
structurally identical question here gets the opposite answer, and the doc comment itself says
why: the hazard is an id that is **not durable at append time**. A code is made durable *in the
same fsync that makes the row durable*, by a new record:

```
WalRecord::VocabularyMint { vocabulary: String, key: String, code: u32 }
```

- **Appended at the end of the `WalRecord` enum.** Variant order is frozen and append-only;
  appending shifts no existing discriminant, so — exactly as the four narrow `WalScalar` widths
  landed in 1fc47ad — there is **no `WAL_VERSION` bump**. An older binary meeting the record is a
  downgrade; postcard refuses the unknown discriminant, which is fail-closed, not a misread.
- **Appended before the `IngestBatch` records it serves, inside the same window close**, so one
  fsync covers both and replay meets the binding before any row that uses it. The close order
  becomes: allocate entities and codes → append mints → append batches → one fsync → apply →
  swap → ack (write-path §2.3, one insertion). A failed fsync applies nothing, as today: no code
  was ever acknowledged, and the drawn codes die with the window — they were never durable, so
  never spent.
- **The row's scalar carries the code** (`WalScalar::U8/U16/U32` at the declared width),
  resolved once and persisted — the same resolve-at-admission-and-persist rule
  `ChangeByEntity` records for entity addressing, and for the same reason: replay must not
  re-derive what admission decided, and a random draw is precisely the thing replay cannot
  re-derive. This is the direct answer to the replay-determinism question: **determinism is
  achieved by recording, not by reproducibility.** The keyed-permutation alternative (a PRF over a
  dense mint counter, deterministic and replayable — the `tessera_id` construction) is declined
  because attributes §3.4 already declined it: the vocabulary table exists anyway, so recording
  buys the property with no key to manage.

Replay applies mint records over the manifest seed in order. A replayed binding identical to one
already held (same vocabulary, key and code) is skipped — the idempotent case a
retry-and-restate design must tolerate. A **conflicting** binding inside the durable prefix —
same key at a different code, or same code under a different key — is corruption of acked state:
refuse to open, the `WalError::WalCorruption` posture, because every row written under either
binding is now of unknowable colour. Likewise a row whose discovered-column code has no binding
anywhere: refuse, do not serve it as "unknown value" — an unexplained code is either a lost mint
record or a future collision, and both are the silent-recolour hazard.

### 3.2 The manifest home — full restatement, the deny-fields precedent

`MANIFEST.json` is written at build and at the fold and immutable between; the between-builds home
must therefore be `SEGMENTS-<n>.json`. Two shapes were considered:

**Declined: a `dict_extents` counterpart** — per-flush files of minted bindings, listed in the
manifest, coalescible. Everything that makes `dict_extents` subtle exists to protect *positions*:
the append-only list order, the no-repeat rule (decision 0042), `coalesce_dict_extents`'s
contiguous-window constraint, the moved-under discard. A vocabulary binding carries its code
explicitly, so none of that machinery has anything to protect; adopting the shape imports the
obligations without the need. It also adds files, digests, a loader walk and an eventual coalesce
policy — mechanism, for a quantity bounded by the code space of a declared width.

**Recommended: a `vocabulary_extensions` field, restated in full at every manifest write**, the
way `deny` and `tombstones` are "serialised from the live overlay at **every** manifest write …
and never carried forward from the manifest being extended" (contracts §2.3). Shape:

```json
"vocabulary_extensions": [
  {"name": "departments", "values": [{"key": "k9-unit", "code": 4711}]}
]
```

- Complete current state per write — contracts §2.3's own property, no chaining.
- `#[serde(default)]`, so every existing bundle opens.
- **`HONOURED_STATE` gains `"vocabulary_extensions"` in the same change that lands the loader
  code acting on it** — the tripwire in `tessera-store/src/manifest.rs` is explicit that a name
  and its code land together, and the failure mode of forgetting is loud (every manifest carrying
  the field makes its partition unready) rather than silent. It is **not** deny-disposition state:
  a reader that stepped past it would show rows whose codes have no key — missing metadata, the
  fail-safe direction — so it joins `HONOURED_STATE` and not `DENY_DISPOSITION_STATE`.
- The loader seeds the live vocabulary view from `MANIFEST.vocabularies` plus the served
  `SEGMENTS`' `vocabulary_extensions` **before WAL replay**, and replay's mint records apply over
  the seed — the established seed-before-replay order (contracts §2.3, and the deny seeding in
  `write.rs`). Since bindings are append-only and never rebound, seed-then-replay is
  order-insensitive except for conflicts, which refuse (§3.1).

**Why restatement is safe against WAL reclamation without any new rotation machinery.** Rotation
runs only at flush publication, *after* the side-manifest write (write-path §4.5), and that
manifest restates the full live extension set. A mint whose rows are still buffered cannot lose
its member: the mint record precedes its batch's rows in the same member (same window, no
rotation inside a window), and the reclaim bound is the oldest surviving buffered row's position,
which sits above the mint in that member — a member is deleted only when *wholly* below the
bound. A mint whose rows have all been flushed is already restated by the flush's own manifest
before rotate runs. So no vocabulary snapshot at the rotated member's head is needed; the
overlay needs one only because the WAL is the overlay's *sole* durable home, and the manifest is
this state's second home by construction.

**The cost, stated rather than waved at**: manifest-write size grows with the discovered
cardinality. Modelled, not measured: at roughly 40 bytes per binding as JSON, a fully-minted
`u16` vocabulary (65,535 values) adds ~2.6 MB to every `SEGMENTS-<n>.json`, written per flush
**and per deny drain** — and deny publication is contractually prompt (contracts §2.3's
publication rule). At `u8` and realistic `u16` cardinalities the cost is noise; at a large `u32`
vocabulary it is not, and the design wants a cardinality alarm (an `overlay_soft_limit` analogue)
before it wants extent files. NOT confirmed by measurement — the crossover at which restatement
loses to extents should be measured if a deployment approaches it, and §9 puts the ceiling with
the owner.

### 3.3 The fold, and decision 0050

The fold writes the next prefix's `MANIFEST.json`; it **folds `vocabulary_extensions` into
`MANIFEST.vocabularies` verbatim** — values appended to the named vocabulary, keys, codes and
labels byte-identical, `reserved` carried — and the new prefix's first `SEGMENTS-<n>.json`
restates an empty extension set. Verbatim is the whole rule: a fold that re-derived, re-sorted or
re-numbered is attributes §3.4's silent corpus-wide recolour, with no error and no digest
mismatch, and the test worth writing is byte-comparison of the folded vocabulary against the
seed plus extensions.

Decision 0050 — a fold invalidates the term index and every fragment — touches none of this.
Codes are not ordinals, index nothing positional, and no cached artefact is keyed by them; the
fold's postings rewrite and fragment invalidation pass over the vocabulary table without reading
it. The one forward obligation: when the attribute postings file exists (⊘), it joins the fold's
pass-2 rewrite exactly as the auth postings do, and *that* file's term ordinals rotate with its
own dictionary — still never touching codes.

Deletion and suppression also touch none of this: a value whose last member is deleted keeps its
key and code (codes retire only by authored `reserved` moves, attributes §2.2), and its
*visibility* going to zero is the derived-not-maintained rule (attributes §3.3) for the postings
work to honour later.

---

## 4. Concurrency — and why `dictionary_moved_under` is not reused

The promotion machinery (`flush::promote`, `dictionary_moved_under` in
`crates/tessera-engine/src/write.rs`) solves this problem: an assignment made as *positions
against a base of length n* is invalid if the base is no longer length n at publication, so a
promoting flush whose dictionary moved under it is discarded. **None of that transfers, because a
scattered code is position-free.** A binding `(departments, "k9-unit", 4711)` means the same thing
whatever was minted before, after or concurrently; there is no base length to move. The
moved-under guard, the promoted-from length, the discard-and-replan — all unnecessary here, and
reusing them would import a liveness cost (discarded work) that buys no safety. This is the
clearest single payoff of separating codes from ordinals.

What remains of the concurrency question is agreement, and the executor already provides it:

- **Two windows minting the same novel key**: windows close serially on the one write executor
  (write-path §1.1); the second close's mint step consults the live view, finds the key bound,
  and reuses the code — the view-first rule, the same shape as `promote`'s dictionary-first
  lookup, without the positional stakes.
- **Two windows minting different keys**: the draw excludes every assigned code (the view, the
  reserved set, and code 0), and the closes are serial, so collision is impossible rather than
  unlikely.
- **Handlers do not mint.** A handler translates keys the generation snapshot already binds and
  forwards unknown keys as keys; two handlers racing on the same novel key both forward it and the
  executor mints once. Minting in handlers is the one place a same-key/two-codes split could
  arise, and it is closed structurally by not doing it.
- **Flush needs nothing.** Rows enter the buffer already carrying codes; the flush writes the
  scalar tail as it does today (`to_scalar_value`, `write_flush_segment`), and publication's only
  vocabulary duty is the restatement §3.2 gives every manifest write. There is no vocabulary
  analogue of a promoting flush.

The draw itself: rejection-sample the width's domain from OS entropy, excluding 0, assigned and
reserved; for `u8`/`u16` near fullness, enumerate the free codes and index uniformly (trivial at
those widths); `u32` never plausibly approaches the fill fraction where rejection sampling
degrades before the cardinality alarm (§3.2) has long since fired. The assigned-code set is a
small bitmap per vocabulary on the `Generation`, rebuilt at open from the seed plus replay — its
**completeness is the never-reuse invariant**; see risk 2.

---

## 5. The reuse map

**Reused as-is (no change):**

| What | Where | Role here |
|---|---|---|
| `Wal` append/fsync/rotate, the window close | `tessera-lifecycle/src/wal.rs`, write-path §2.3 | carries the mint record; one fsync covers mint + batch |
| `Generation` swap discipline | write-path §1.2, `tessera-engine/src/lib.rs` | the live vocabulary view hangs off the generation like the dictionary does |
| `ManifestVocabulary`, `ManifestVocabularyValue` | `tessera-store/src/manifest.rs` | the extension field reuses the value shape; the fold appends into the table unchanged |
| `WalScalar` widths, `to_scalar_value`, `write_flush_segment`'s scalar tail | `tessera-lifecycle`, `tessera-engine/src/flush.rs`, `tessera-store/src/write.rs` | rows carry codes exactly as they carry any scalar today |
| Seed-before-replay order | contracts §2.3, the deny seeding in `write.rs` | the vocabulary seed follows the deny fields' order verbatim |

**Parameterised (a real change to an existing thing):**

| What | Change |
|---|---|
| `Schema::parse` / `compile_category` (`tessera-build/src/schema.rs`) | lift the `discovered` refusal; `Vocabulary` gains a kind; the batch build mints for discovered vocabularies through the same routine ingest uses, recording into `MANIFEST.vocabularies` (attributes §5's build half); `values_key` seeding of a discovered vocabulary already parses, and its codes join the assigned set |
| `DeclaredScalar` (`tessera-store/src/manifest.rs`) | gains the expected-wire-type function (`utf8` iff `vocabulary.is_some()`), the **one** parser rule, reached via the engine re-export |
| `parse_ingest_batch` (`tessera-server/src/control.rs`) | validates category columns against the derived wire type; translates declared keys; carries unknown discovered keys to the executor; the declare-then-use 422 |
| `WalRecord` (`tessera-lifecycle/src/wal.rs`) | one appended variant, no version bump |
| `SegmentsManifest`, `HONOURED_STATE`, the loader | the `vocabulary_extensions` field, its serialiser at every manifest write, its seed |
| The fold's manifest assembly | extensions folded verbatim into `MANIFEST.vocabularies` |

**Not reused, with the reason in each case:**

| What | Why not |
|---|---|
| `DictWriter`, `DictStreamWriter`, `Dict`, `coalesce_dict_extents` (`tessera-authz/src/dict.rs`) | every one is ordinal-positional by construction — a record's meaning *is* its position — and attributes §3.4's whole point is that a code must not be a position. Embedding codes in the records would keep the file format and discard every property the format exists for |
| `write_postings`, `PostingsReader`, `PostingsSpool` (`tessera-authz/src/postings.rs`); `DeltaTier`, `write_delta_tier`, `coalesce_delta_tiers` (`tessera-authz/src/tier.rs`) | postings serve the `filter` placement, which is refused at parse (⊘). A discovered vocabulary under `render` needs no membership set. These are reserved for the attribute-postings namespace of attributes §3.5, where they are to be reused **verbatim** — same tagged records, same readers — under the attribute dictionary's own dense TermIds. Building that namespace now would be scope this design does not need (§6) |
| `DescriptorResolver`'s extension-id scheme (`tessera-lifecycle/src/buffer.rs`) | exists because a term ordinal is not durable until its flush promotes it, so novel descriptors need unsatisfiable placeholder ids. A code is durable at the window close; there is no placeholder interval to bridge |
| `flush::promote`, `dictionary_moved_under` (`tessera-engine/src/flush.rs`, `write.rs`) | guards a positional assignment against a moved base; a scattered code has no base to move (§4) |

**Genuinely new:** the mint routine (draw + record, shared by build and ingest so exhaustion is
one predicate — a `BuildError` there, a 422 here); the per-vocabulary live view on the
`Generation` (key→code map plus assigned-code bitmap); `WalRecord::VocabularyMint`; the
`vocabulary_extensions` field with loader and fold step; the ingest translation and
declare-then-use check.

---

## 6. What this deliberately does not build

- **The attribute dictionary and postings file** (attributes §3.5). Nothing in `render` plus
  `discovered` needs a membership set, and `filter` stays refused at parse. Building the second
  namespace "while we're here" would land authorisation-adjacent machinery with no consumer and
  no test that can exercise it — decision 0013's exact hazard.
- **Positional vocabulary extents, their coalesce, and a moved-under guard** — removed by the
  restatement design (§3.2). This is the memo's main mechanism-removing simplification: the
  entire "extents" concept does not apply to a value that carries its own identity.
- **A keyed permutation over the code space.** Deterministic minting via a PRF would remove the
  WAL record; attributes §3.4 already declined the key, and the WAL record is one variant.
- **A vocabulary snapshot in the rotated WAL member's head.** Unnecessary — §3.2's
  restatement-before-reclaim argument; adding one anyway would be a second durable home whose
  disagreement with the first is a new failure mode.
- **`/v1/categories`** and any listing enforcement. ⊘ stands: `listing` remains recorded, not
  enforced; a discovered vocabulary's keys are operator-visible only through the manifests until
  that endpoint is designed against contracts §3.2's `/v1/meta` reconciliation (attributes §3.8).
  Minting does not require it.
- **A code-carrying wire form.** No caller supplies codes, for any vocabulary kind, on any plane
  (pending the owner ruling in §9 on whether declared vocabularies keep a transitional form —
  the recommendation is no).
- **Any change to code 0, widths, or exhaustion semantics** — ruled; restated as constraints.

---

## 7. What this costs

- **WAL:** one appended `WalRecord` variant (`VocabularyMint`). No `WAL_VERSION` bump — appending
  shifts no discriminants; the downgrade direction fails closed on the unknown discriminant. No
  `WalScalar` change. The window close gains a mint step, and mint appends precede batch appends.
- **Manifest:** one new `SEGMENTS-<n>.json` field, `vocabulary_extensions`, `#[serde(default)]`;
  `HONOURED_STATE` gains its name beside its loader. `MANIFEST.json` is shape-unchanged (the
  `vocabularies` table already exists); the fold gains the fold-in step. Manifest-write size grows
  with discovered cardinality (§3.2 — modelled ~40 B per binding; a ceiling is an owner decision).
- **Wire (admin plane):** category columns switch from codes-at-width to `utf8` keys — a breaking
  change to `/control/ingest`'s batch schema for category columns, licensed by decision 0048;
  contracts §3.4 and attributes §5 amended; the `build_attributes.py` fixture and the ingest and
  attribute-tail tests that carry code-typed columns are rewritten.
- **Build:** the batch pipeline mints for discovered vocabularies (same routine), and the
  `discovered` refusal in `schema.rs` lifts.
- **New files on disk: none** — the recommended design adds a manifest field, not a file kind.
- **No `bundle_format` bump**: every added field is `default`-tolerated by existing readers, and
  the one reader that must *act* on the new field is gated by `HONOURED_STATE`, which is the
  mechanism built for exactly this.

---

## 8. Risk register — silent failures first

1. **Rebuild recolour (silent, corpus-wide).** A full `tessera build` re-run against
   `schema.toml` has no source for previously minted codes — the schema pins nothing for a
   discovered vocabulary — so a rebuild mints fresh random codes, and every consumer holding the
   old legend, and every expectation formed against the old bundle, silently recolours. Inside
   one bundle nothing is wrong, which is what makes it silent. This is the identity-key lineage
   problem again (`--carry-id-key-from`), and it needs the same shape of answer — owner decision
   1. Until ruled, a build declaring a discovered vocabulary without a lineage source should
   refuse, exactly as a build without an id-key source refuses.
2. **Incomplete assigned-set reconstruction re-mints a live code (silent recolour of old rows).**
   Never-reuse is enforced by the in-memory assigned set; if the seed misses a source —
   `MANIFEST.vocabularies` but not `vocabulary_extensions`, or extensions but not replayed mints,
   or a seeded `values_key` file's codes — a later draw can land on a code that already colours
   rows, and every functional test over fresh state passes. The invariant wants a direct test:
   open a bundle with bindings in all three homes and assert the draw domain excludes all of
   them; plus the replay-conflict refusal (§3.1) as the backstop that turns the worst case loud.
3. **Mint outside the executor (silent).** Two handlers racing a novel key yield two codes for
   one key — rows split between them, and whichever binding survives recolours the other's rows.
   Closed structurally (§4: handlers never mint); the risk is a later "optimisation" moving
   translation forward. Worth a comment at the mint site of the `is_deleted` kind: the statement
   of *where* minting may happen is the invariant-bearing half.
4. **RNG density regression (silent).** A first-fit fallback, a seeded test RNG leaking into
   production, or draw-from-counter "temporarily" reintroduces the cardinality disclosure §3.4
   closes — and no functional test notices, because dense codes work perfectly. Wants a test that
   minting k values into an empty `u16` space never yields exactly 1..k (probability ~0 under a
   correct draw), and a named source (OS entropy) at the draw site.
5. **A fold or manifest writer that re-derives instead of copying (silent recolour).** The fold
   step and every restatement must be byte-faithful to the seed plus extensions; a writer that
   "normalises" — sorts values by key and re-numbers, or round-trips through the schema compiler
   — is attributes §3.4's central hazard. Pinned by a byte-comparison test across build → mint →
   flush → fold → reopen.
6. **The expected-wire-type rule transcribed twice (silent-adjacent).** The 1fc47ad lesson: two
   copies of the type table disagreed and were unreachable until the declaration was non-empty.
   Two copies of the category-wire rule would let a `u16` category be validated as a plain `u16`
   somewhere — codes accepted raw, the §2.1 hole reopened without anyone deciding to. One
   function on `DeclaredScalar`, and the server reaches it through the engine.
7. **Wire-absent conflation (silent, per-row).** Null against the empty string against missing:
   if the empty string ever mints, a typo becomes a value with a posting-shaped future; if null
   silently becomes code 0 where the caller meant a value, rows are quietly absent. Refuse the
   empty string loudly; rule the null form (§9).
8. **Exhaustion mishandled (loud if right, silent if wrong).** A wrap or widen is a recolour;
   the typed 422/build error is ruled. One test per width at the boundary (255th, 65,535th mint).
9. **`HONOURED_STATE` misstep (loud).** Field without loader means every carrying manifest is
   unready (an availability outage, not a leak); loader without the field name, the same. The
   manifest tests' existing tripwire pattern covers it; land both in one change.
10. **Manifest bloat at high discovered cardinality (loud-ish, latency).** Restatement cost rides
    every deny publication, which is contractually prompt. Modelled only — NOT confirmed by
    measurement; the cardinality alarm (§3.2) is the guard, and its threshold is owner decision 3.

---

## 9. Decisions the owner must make

Each phrased to be rulable without reading the code:

1. **Rebuild lineage for minted codes.** When the corpus is rebuilt from source and the schema
   declares a discovered vocabulary, where do the previously minted codes come from — a
   `--carry-vocabulary-from <bundle>` flag (mirroring `--carry-id-key-from`), a required export
   of the discovered vocabulary to an authored values file (graduating it to `declared`) before
   any rebuild, or a refusal to rebuild discovered vocabularies at all until one of those exists?
   Until ruled: refuse.
2. **One wire form, or two.** Is the `utf8` key the only wire form for every category column —
   including `declared` vocabularies, breaking the branch's just-landed code-at-width ingest —
   or may a declared vocabulary transitionally accept codes? Recommendation: keys only;
   decision 0048 makes the break free today, and a second form is a permanent second code path.
3. **A discovered-cardinality ceiling.** At what vocabulary size does the system alarm, and is
   there a size at which minting refuses outright (independent of code-space exhaustion)? The
   manifest-restatement cost (§3.2) and the plan step's discovered-plus-width warning
   (attributes §2.3) both want a number; none exists, and it should be an alarm first unless
   ruled otherwise.
4. **The wire spelling of absence.** Is a null in a category column accepted as *absent* (stored
   as code 0), or is null a 422? Storage-side absence (code 0) is ruled; the wire form is not.
5. **Minted-key exposure before `/v1/categories` exists.** A discovered vocabulary's keys are
   data-derived and `listing = "public"` is forbidden for it — but the keys now sit in
   `SEGMENTS-<n>.json` and, after a fold, `MANIFEST.json`, which are operator artefacts. Confirm
   that the bundle-holder boundary (the I10/decision-0014 posture: the bundle holder is trusted)
   covers data-derived keys in manifests, or rule that discovered keys need separate
   manifest-side handling before minting ships.

---

## Appendix — the storage map in one table

| Where | Carries | Form |
|---|---|---|
| ingest wire (`/control/ingest`) | key | `utf8` column, validated against the declaration-derived expected type |
| WAL `VocabularyMint` | (vocabulary, key, code) | the birth record; same fsync as the batch |
| WAL row scalar | code | `WalScalar` at declared width, resolved at the close |
| `columns.arrow` tail | code | declared width, unchanged from 1fc47ad |
| viewer wire | code | unchanged; the legend is `/v1/categories`' future problem (⊘) |
| `MANIFEST.vocabularies` | build-time and folded bindings, plus `reserved` | immutable between builds and folds |
| `SEGMENTS-<n>.json` `vocabulary_extensions` | every post-build binding | full restatement per write; folded in and emptied at the fold |
| attribute dictionary and postings (⊘ unbuilt) | dense TermIds over `(column, key)` descriptors | never a row, never the wire; joined to codes through the key only |
