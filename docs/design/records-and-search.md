# Records and search — design

**Date:** 2026-08-14 (r8 — the text family built end to end, and the corpus's status claims
corrected against it; Appendix R)
**Status:** **Provisional — nothing remains open; promotion awaits only the §13 amendments pass,
which is an editing round rather than a decision.** Its **first three epics are built** — §2's
declaration, §3's record blob through the whole lifecycle, drill-down's assembly from the three
homes, §6.2's row-space route, §4.3's keyword family in place of the deleted `utf8` column, and
§4.4's text family: the icu4x analyser, the token index through all three of its producers, and
`match` with its m-of-n form and `phrase` verified against the record blob. What remains is
**multi-value** (§5) and **scoring** (§4.5) — in that order, and every mechanism below is ⊘ unless
it names an existing one. The adversarial review
([`2026-08-12-records-and-search-review.md`](../evidence/memos/2026-08-12-records-and-search-review.md))
found the security argument sound, the read-side cost argument sound with figure corrections, and
the seams not yet survivable; every finding was applied per its recommendation, and the six
then-open rulings were made the same day (owner, 2026-08-12 — §12, and decisions
[0067](../decisions/0067-term-timing-is-accepted-for-text-and-keyword-postings.md),
[0068](../decisions/0068-a-row-space-operand-bounded-by-the-requests-domain-is-admitted.md),
[0069](../decisions/0069-filter-do-not-rank-sharpens-to-no-corpus-global-statistics.md)).
§11's scale measurements run inside the epics per §12's dataset amendment and gate the corpus-scale
claims and the large dataset tiers, not the work's start. The evidence base is
[`2026-08-12-record-and-searchability.md`](../evidence/memos/2026-08-12-record-and-searchability.md)
(cited as **memo §n**), [`2026-08-12-filter-placement.md`](../evidence/memos/2026-08-12-filter-placement.md)
(**placement §n**), and seven probe campaigns — four taken for the draft:
[`2026-08-12-string-storage/`](../../probes/2026-08-12-string-storage/),
[`2026-08-12-filter-placement/`](../../probes/2026-08-12-filter-placement/),
[`2026-08-12-keyword-and-list-storage/`](../../probes/2026-08-12-keyword-and-list-storage/),
[`2026-08-12-phrase-cost/`](../../probes/2026-08-12-phrase-cost/) — and three taken against built
code, which is where §4.3's and §6.4's shipped figures come from:
[`2026-08-12-epic1-measurements/`](../../probes/2026-08-12-epic1-measurements/),
[`2026-08-13-keyword-dict/`](../../probes/2026-08-13-keyword-dict/),
[`2026-08-13-contains-recovery/`](../../probes/2026-08-13-contains-recovery/), with the `utf8`
retirement fence ([its memo](../evidence/memos/2026-08-13-utf8-retirement-fence.md)) as the
baseline the last two measure against.
**Reads against:** architecture §4 (I2, I7, I9, I12), §8.2–§8.3, §10.3;
[`filter-index.md`](filter-index.md) §1–§2, §5–§6 (**index §n**);
[`filter-surface.md`](filter-surface.md) §2, §4–§5, §7 (**surface §n**);
[`per-point-attributes.md`](per-point-attributes.md) §2–§3 (**attrs §n**);
[`write-path.md`](write-path.md) §2.3, §4.3, §5.4; decisions
[0013](../decisions/0013-mark-specified-vs-implemented.md),
[0021](../decisions/0021-rust-not-jvm.md),
[0039](../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0048](../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md),
[0062](../decisions/0062-filters-compose-as-a-boolean-tree-inside-the-candidate.md),
[0063](../decisions/0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
[0064](../decisions/0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md),
[0065](../decisions/0065-the-inverse-permutation-is-stored-for-the-filtered-viewport.md),
[0066](../decisions/0066-none-of-requires-a-value-and-names-one-column.md),
[0067](../decisions/0067-term-timing-is-accepted-for-text-and-keyword-postings.md).
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **records §n**.

---

## 1. Summary

This design makes per-item data general: a field of any of five families — **number**, **datetime**,
**category**, **keyword**, **text** — declared by what it *is*, whether it **renders**, and whether
it is **indexed**, with everything else derived. It replaces the `used_for` placement set, gives
every declared field a record the system can return at drill-down, gives strings the two mechanisms
they actually need (an exact identifier and analysed prose in any script), admits multi-valued
fields in every family that does not render, and stages masked scoring and exact phrase on one
shared upgrade. Tessera stops being a map with a few filter columns bolted on and becomes a
permission-masked record service that is optimised for serving maps — without widening the query
surface that keeps Appendix C enumerable: every capability below enters as an operand under the
filter contract (§8.2), composed by the boolean tree decision 0062 fixed.

Three rules organise all of it.

**Every field lives in exactly one of three homes, and the record always exists.** A field declared
at all is a field the system stores and can return: a rendered field lives in the hot column, an
indexed field lives in its family's entity-space structure — which also answers `entity → value`,
so nothing is stored beside it — and everything else lives in a **per-entity compressed record
blob** (§3), the cheapest bytes available. `index = false` is therefore a real decision rather than
a default nobody notices: it trades the operand and the aggregate surface for record-blob storage.
`inspect` disappears as a placement because there is nothing left for it to opt into. One family
stands outside the rule: **a category's entity-space structures exist whatever its flags say**,
because the vocabulary machinery is what a category *is* (§3, §4.2 — review finding B1's
disposition).

**The mechanism follows the vocabulary, not the type** (memo §4). A category's closed vocabulary
earns per-value postings (built). A keyword's open, exact vocabulary earns a sorted per-layer
dictionary with an ordinal column — measured 2.18–3.61× smaller than the flat `utf8`
column it replaces, **>2× modelled on every shape for the decodable format** (§4.3) — while
turning string equality and prefix into fixed-width scans. Prose earns a token index that is
**3.5× smaller than the flat column it replaces** and is the only mechanism that serves its
aggregate surface. Numbers and datetimes keep the native masked scan, which needs nothing.

**The hot path is untouched.** Nothing here changes row-space layout, tile serving, mask
composition, or the viewport's selection route. The one change near the render path — serving a
filtered viewport from the render column itself (placement §2) — runs inside the per-tile sweep
and touches no entity-space artefact, which is what it is *for*: a rendered column is filterable
with no second copy. It is **not** generally the cheaper route, the probe's 7–1,269× having been
refuted on built code ([the epic-1 measurements](../evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md)); §6.2 carries the figures.

The families, and where each mechanism stands:

| family | stored as (entity space) | searched by | record | state |
|---|---|---|---|---|
| **number** | native flat column + presence | masked scan: `eq`, `in`, `range` | the column itself | scan **built** |
| **datetime** | `timestamp_us` (`i64`) flat column + presence | masked scan: `eq`, `in`, `range` | the column itself | scan **built** |
| **category** | code column + vocabulary + derived postings — **always** (§4.2) | scan; postings per decision 0063 | the column + vocabulary | **built** |
| **keyword** | sorted per-layer dictionary + `u32` ordinal column | dictionary resolve → fixed-width scan; ⊘ postings for the privileged tail | dictionary + ordinals | dictionary, ordinals and every operator **built**; ⊘ **no producer emits keyword postings**, so `eq` is the ordinal scan (§4.3, §6.1) |
| **text** | token dictionary + per-token postings | `match` (+ m-of-n) over token postings; `phrase` verified against the blob (§4.5) | its rows in the record blob | **built** — declaration, analyser, all three producers, `match` and `phrase`; ⊘ no scoring (§4.5), and `none_of` is refused (§4.4) |

An unindexed, unrendered field of any family but category lives in the record blob and has no
operators. `bool` remains the degenerate number. The `utf8` family is **deleted** and `keyword`
replaces it (§4.3); decision 0048 makes that a rename plus a rebuild, with no compatibility
surface.

---

## 2. The declaration

A field is declared once:

```toml
[[attribute]]
name   = "submitter"
type   = "keyword"          # number widths | timestamp_us | category | keyword | text
index  = true               # build the family's search structure; default false
render = false              # draws on the map; default false
multi  = false              # more than one value per item; default false
# category-only, unchanged: width, vocabulary, listing, values / values_key / values_of
```

**`index` is the word deliberately borrowed.** It is what Elasticsearch, OpenSearch, SQLite FTS and
Typesense all call exactly this switch, and it passes decision 0062's borrowing test — adopt a
familiar word only where its semantics transfer intact — where `filter`/`should` failed it: an ES
reader's expectation of `index: false` (*not searchable, still stored, still returned*) is
precisely the record blob's behaviour, and attrs §4.5 already names ES mappings as the convention
this surface stole from. Two divergences are recorded rather than discovered. An ES field with
`index: false` can still feed aggregations through doc_values; here an unindexed field feeds
nothing — but Tessera aggregates arise only *through* filters, so the reader's model survives. And
`index = true` on a number derives a scanned column, not an inverted structure — the key names the
capability; the mechanism stays derived and reported, never asked for (attrs §2.1).

Everything else derives. What `used_for` used to spell becomes:

| today | becomes |
|---|---|
| `used_for = ["render", ...]` | `render = true` |
| `used_for = ["filter", ...]` | `index = true` |
| `used_for = ["inspect", ...]` | nothing — the record always exists (§3) |

The collapse removes a double store the old surface forced a caller to pay for: `filter` and
`inspect` were two faithful copies of one value, chosen separately (memo §2). It also makes
placement §5's first ruling a consequence rather than a rule — a rendered fixed-width column
already affords a row-space search over the request's own rows (§6.2), so `render = true` makes it
filterable at no additional storage.

**Refused at parse**, each naming itself per decision 0013: `render` on `keyword` or `text`
(non-fixed-width — attrs §4.3's rule, unchanged); `render` with `multi = true` (decision 0039's
fence, restated at §5); `record` as a column name (it is the blob's namespace — review N10); and
the category-specific refusals of attrs §4.3, all unchanged. `type = "utf8"` is refused with a
message naming `keyword` and `text` and the difference.

**A rendered number was refused `index` for one epic, and is not any more.** Under store-once the
hot column is the only copy, and it stores an absent number as the type's zero — so a range
containing zero matched every item with no value, and the surface refused the combination rather
than answer wrongly. Decision 0064's render half closes that on the server side: a presence bitmap
beside the column, written by both builds, flush, merge and the fold, and read by the row-space
route. Every fixed-width family is now filterable from the hot column. What 0064 still defers is
the wire and the client — the points batch cannot say "absent", so a client draws an absent number
at zero while the filter treats it as having no value, a narrowing disagreement recorded there.

**Required:** `name`, `type`. Categories additionally require `width`, `listing`, `vocabulary`
exactly as today — disclosure and migration controls do not default, and they remain meaningful at
every flag combination because a category's entity-space structures always exist (§4.2). Everything
else defaults to `false`: the cheapest placement, made more expensive only by an explicit word.

---

## 3. The record, and its three homes

**Every declared field can answer `entity → value` at interaction cadence**, because a field a
caller bothered to declare is a field they will eventually click on. §10.3 prices that cadence:
per-interaction data may cost a random read; it may never cost residency or per-mark work. A
field's value bytes live in exactly one home:

| home | who lives there | `entity → value` route |
|---|---|---|
| the hot column (row space, per slice) | `render = true` | `tessera_id → entity → row`, one array read |
| the family's entity-space structure | `index = true` — and **every category** (§4.2) | one array index; ordinal → dictionary for keyword |
| **the record blob** | everything else — including every `text` field's values | one block read |

The store-once consequence, as arithmetic: for every family except `text`, `index = true` **adds no
second copy** — the search structure is the record. For `text` it adds the token dictionary and
postings (~24 B/entity measured on titles), and buys the only mechanism that serves its aggregate
surface (§6.3).

**The record blob** is the one new storage format, and it is deliberately the
`_source` shape every ES reader knows: per entity, the blob-resident fields serialised as one
compact self-describing row — field tag, then the typed value; a `multi` field is a
length-prefixed list — rows concatenated in entity order, cut into zstd-compressed blocks with a
**256 KiB target**, chosen at the string-storage probe's 2.44× point
([`string-storage`](../../probes/2026-08-12-string-storage/) arm 1). Through the built writer and
reader the shipped format measures **3.00× on a mixed row at 236–270 µs per random single-row
read** ([the epic-1 measurements](../evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md)); the probe's 169 µs was optimistic for a 256 KiB block rather than the reader being
slow, and the spread across the three row shapes is not ordered by row size, so it is the block
read and not the row that the figure is about. **A row never splits across
blocks**: a block holds one or more whole rows, and a row larger than the target gets an oversized
block of its own — the target is a target, not a cap.

**Addressing is has-row rank, and it is specified because both obvious readings of an earlier
draft were wrong** (review B5). A **has-row Roaring bitmap** marks the entities that have a blob
row; an entity's rank in it indexes a compacted array of `u32` within-block offsets; a block
directory of `(compressed offset, first rank)` locates the block by binary search. An entity with
no blob-resident field is absent from the bitmap and occupies nothing. A blob **field** needs no
per-field presence structure — a field's absence is its absence from the row — but the blob as a
whole carries the one has-row bitmap; the two statements are about different things and both hold.
One block read returns an entity's whole residual record; drill-down assembles the rest from the
other two homes by array index.

**The blob read is fail-closed against its one new failure class** (review B6). The other two
homes are positional, so there is no offset to get wrong; the blob's indirection is new, and a
build or fold defect the digest cannot catch — digests cover bytes, not addressing consistency —
would otherwise serve a *neighbour's* record for a visible entity, from blocks that also hold
entities the principal cannot see. So: every offset and row length is bounds-checked against its
block; each row carries its entity id as a discriminant, checked at read and **never serialised
to any client** (I10 as corrected by 0065 — the blob is an index internal, not a gather artefact);
a mismatch refuses the request rather than answering. §10's catalogue gains the block-boundary
cases.

Three honesty notes travel with the format. The mixed row's ratio was *assumed* to match the
per-column 2.44× and is now **measured better than it**: 3.00× against a 2.54× title control
through the same writer, the shared context between neighbouring rows buying more than
interleaving costs, with addressing at **4.13 B per has-row entity** ([the epic-1 measurements](../evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md)). The
blob-versus-dictionary comparison is per column shape (review N7): on a near-sequential identifier
(`id`) the blob's compressed content is ~0.6 B/entity and the whole blob row ~4.6 B under this
addressing — *cheaper* than DICT+C's 6.1 — while on `doi`/`submitter` shapes the blob costs ~11.7
and the dictionary wins decisively; "`index = true` is cheaper *and* searchable" holds for the
latter shapes, not the sequential-identifier one. And flipping a field to `index = true` later is
a derivation pass over the blob — attrs §2.2's existing "build pass, no row rewrite" class —
where under the old surface it was free; that is the price of not storing every field twice.

**A column the scan reads is never compressed; the blob is never read by a query.** That rule is
what the whole section rests on (memo §3): the same probe measured the best block ratio at 29× the
contiguous scan constant, so compression is not a trade a scanned column can price — and an
exactly-answering index is what frees the bytes to take it.

**The conformance relation comes from the fixture's inputs, not from this artefact** (review B7).
The shipped oracle deliberately never opens the attribute artefact: its values come from the
fixture's own generation functions, so a build that wrote wrong bytes and then served consistently
by them *disagrees* with the oracle instead of being agreed with — strictly stronger than reading
the artefact, and this design inherits that construction rather than the weaker one an earlier
draft claimed. The one narrow artefact-level check the blob adds is its own **addressing
self-consistency** — rank, offsets, discriminants — which the fixture cannot see and B6's
refusals depend on. Within the system, the artefact-of-record rule stands as stated: derived
structures are rebuilt from the record, never trusted beside it.

---

## 4. The families

### 4.1 Numbers and datetimes

Unchanged in mechanism: a native-encoding flat column, compared natively, scanned under the
candidate (index §3 — built). A `datetime` is `timestamp_us`, an `i64` of UTC microseconds; it is a
number with a name and earns no mechanism of its own. Absence is decision 0064's presence bitmap
beside the column; NaN matches nothing by IEEE comparison, with no rule written for it. Operators:
`eq`, `in`, `range` — set membership at index §2.2's measured table-or-sorted-list cost. Zone maps
and bit-sliced indexes remain declined for index §3's reasons — though §4.5's masked
average-length is the aggregate argument index §3 reserved bit-slicing for, arriving on schedule.

A rendered number or datetime additionally has the row-space route (§6.2), its absence carried by
0064's presence bitmap beside the hot column — without which `ScalarValue::or_render_placeholder`
stores absent as zero and a range containing zero would match every item with no value — the
defect fixed on the entity path on 2026-08-11, and closed on the row path by the same bitmap.

### 4.2 Categories

Unchanged in mechanism — the code column, the vocabulary as a first-class object with pinned
scattered codes, `listing` as the disclosure control, derived per-value postings, decision 0063's
route split — and **exempt from store-once and from the blob, by construction rather than by
exception** (review B1's disposition; owner, 2026-08-12): a category's entity-space code column
and derived member postings exist **whatever `render` and `index` say**. Surface §7's ruling
already states the principle for postings — *a membership set is not an optional filter placement;
it is what a category is in entity space* — and the machinery is built on it: the `per_viewer`
membership gate, `/v1/categories`' answer from postings plus the value-column extent tail, and
0063's public-listing filter route all run on those structures, and the row-space route cannot
answer the membership question (placement §2.1's third bound, which an earlier revision silently
dropped). So for a category, `render` adds the hot column, `index` offers the filter operand, and
the entity-space structures are the floor — which is exactly what the built system stores for a
rendered category today, so nothing regresses. `listing` and `vocabulary` keep their meaning at
every flag combination. The second copy this keeps is bounded by the code width: 1–4 GB per
rendered category column at 10⁹, the price of the vocabulary machinery rather than of a filter.

**The floor is the readers', not the family's, and one shape falls outside it** (owner,
2026-08-12, narrowing the ruling above). Every reader named in the argument is a reader of a
*membership* set: the `per_viewer` gate, `/v1/categories`, 0063's public-listing route. So the
floor is exactly what the build grants it to — an `index`ed category, or a `per_viewer` one — and
a **`public` category declared with neither flag has no reader and no floor**. Stating the
exemption as the whole family's cost that field its only home: unrendered, unindexed, and skipped
by the blob as a category, its values were stored nowhere and the declaration was accepted. **A
field is blob-resident exactly when it has no other home**, categories included, which is the
question both placement passes ask and what makes them exhaustive between them (§3). Granting the
floor unconditionally instead was the declined alternative: it buys structures nothing reads, at
1–4 GB per column at 10⁹.

The contrast with §4.3 is the design argument for both, so it is stated once, here: **a category's
value is a vocabulary entry — durable, served, authorable; a keyword's value is row data, and its
term identity is a per-layer accident.** A category code is minted once, scattered, pinned and
never reused, because it is stored in rows and served to clients. A keyword ordinal is a position
in one layer's sorted dictionary, rebuilt whole at every fold, never durable and never crossing the
trust boundary. That difference is what keeps C11 — a manufactured, durable identity whose
collision or reuse discloses membership — structurally out of the keyword and text families: the
hazard needs an identity that outlives the artefact, and none exists (memo §6.1).

### 4.3 Keywords

**Built.** A `keyword` is a short string with no vocabulary, matched exactly: an identifier, an
order number, a hostname, a submitter. It replaces `utf8`, whose operators it keeps — `eq`, `in`,
`prefix`, `contains` — with the same byte-exact semantics and a different cost profile.

**The swap was measured against the column it replaced, before that column was deleted, and it is
not free** ([the retirement fence](../evidence/memos/2026-08-13-utf8-retirement-fence.md), *measured* at 2.4M on real arXiv columns through both shipped
implementations). Storage falls **2.16–3.56×**. `eq` improves up to 8.1×, `in` up to 5.3×,
`prefix` up to 4.2× on a near-unique column, and all three are level to slightly better on a
scattered candidate. **`contains` is slower everywhere it was measured — 1.5× on a scattered
candidate to 71× on a contiguous one**, and the two costs have different shapes: the flat scan was
linear in the candidate and searched a run's concatenated bytes as one region, while the broad
dictionary route is flat in the candidate and linear in the vocabulary. So the loss is largest
exactly where the candidate is small, which is the per-keystroke cell, and smallest on a scattered
whole-corpus one. At 10⁹ over a unique vocabulary that models as ~1.1 s against ~16 s.

That band is the **shipped** one, and it took a second campaign to get it
([`contains-recovery`](../evidence/memos/2026-08-13-contains-recovery.md)). The fence's own arms
were reimplementations whose substring searchers were hoisted out of their loops, where the shipped
routes constructed one per key and per candidate entity, and its best cell additionally used an
ordinal bitset it flagged as bench-local. The tree was therefore **2.5–144×** when the fence ran.
Three changes have since landed — `tessera_filter::KeyMatcher`, the narrow route's block walk, and
the domain-sized ordinal table — and the shipped route now reaches the band the fence recorded.
Nothing else about the fence's account changes; the routes always answered correctly, and every
other operator's figures were unaffected.

**The last of those three is a disclosure fix and belongs in §8's terms, not §6.4's.** Both routes
ended by binary-searching a sorted list of matching ordinals per candidate slot, and that list's
length is the number of dictionary keys carrying the substring — a corpus-wide count, including
keys no visible entity carries, that a caller moves by choosing a fragment. Measured per candidate
slot: 1.41 ns where five keys matched, 27.72 where 399,554 did. The traversal was identical
throughout, so the scan-work harness could not see it. A table over the dictionary's ordinal domain
is O(1) per slot and the same size whatever matched — 0.34–0.38 ns across that span — which is the
`u8`/`u16` argument this file already makes for `in`, applied where the set's size stopped being
the caller's own.

The regression is a price this design accepts rather than one it hides: `contains` on a keyword is
a substring predicate over values the format deliberately elides shared prefixes from, and the
alternative — keeping a flat copy of every string column so one operator stays fast — is the
second copy §3's whole argument declines. What the measurements change is that the price is now a
figure rather than a hope, and §6.4 carries it. The recovery memo costs four further levers, none
needing a new artefact, and states which of them the small-candidate cell actually turns on: not
the broad route's constants but the narrow route's, where `key_of` decodes half a restart block per
candidate entity to return one key.

**Storage is a sorted dictionary plus an ordinal column, per layer.** Each layer — the base build
and every extent — holds its own front-coded, lexicographically sorted dictionary of the distinct
values it contains, and a `u32` ordinal per present entity naming that value's position in *that
layer's* dictionary. Presence is the standard bitmap. Measured on real identifier-shaped columns
([`keyword-and-list-storage`](../../probes/2026-08-12-keyword-and-list-storage/)): **6.1 B/entity
against the flat column's 17.8** on a fully unique column, 5.7 against 22.3 on a repeat-heavy one,
7.7 against 33.2 on a sparse one — the win coming from front-coded shared prefixes, interning of
repeats, and a `u32` ordinal replacing an `i64` offset. **Those dictionary bytes are a floor, not
the format's cost** (review N3): the probe's front coder stores no suffix lengths and no restarts,
so it is not decodable as stored. **The built format costs 2.0–2.9 B/key over that floor and the
whole family measures 2.18–3.61× smaller than the flat column** ([the dictionary campaign](../../probes/2026-08-13-keyword-dict/results.md), *measured* on the same three
arXiv columns through the shipped writer) — against the ~2.2–3.9× this modelled, so marginally
under at both ends, and the sign is safe everywhere. The model's "~1–2 B/key more" holds for the
two identifier columns and is exceeded by `doi` at +2.9, the excess being the prefix elision a
block's first key gives up; it therefore scales with how much neighbouring keys share, which is
the one direction the model did not carry. The per-key figure is also shape-scoped, and it is the
**built** format's at the shipped restart interval of 16 rather than the probe's floor: `id`
measures **4.15 B/key**, `doi` **6.60**, and `submitter` **9.61** (2.2 B/entity over 542,489
distinct in 2.4M) — repeat-heavy columns pay more per key and far less per entity. The floor's own
2.11 / 3.70 / 7.62 must not be quoted as the format's cost (review N3); every figure in this
paragraph is the writer's. The postings-plus-compressed-record layout that is right for prose was measured **wrong**
here — worse than the flat column on the repeat-heavy shape — which is why keyword and text are
two families and not one. ⊘ One further caveat: the figures are arXiv-shaped; a prefix-free key
set (UUIDs) front-codes to nearly its raw bytes, and the layout then merely ties the flat column.

The dictionary format is front-coded blocks with periodic restarts: binary search over restarts,
sequential decode within a block, a prefix's ordinal range found by two searches. An FST would
serve too ([`dict-fst`](../../probes/2026-08-03-dict-fst/) measured one for the authorisation
dictionary); front-coded blocks are chosen because the byte target is met with the restart
overhead counted, the build is an append over sorted keys rather than an automaton construction,
and nothing here needs infix sharing. Revisit only with a measurement.

**Evaluation.** Sortedness makes every fast operator an ordinal question, answered per layer and
unioned — disjoint by I9, exactly as every layered scan composes today (index §5):

- `eq` — resolve the needle in the layer's dictionary; scan the layer's ordinal column for that
  ordinal under the clipped candidate. The scan is the **fixed-width** scan at its measured
  constants (~0.25–0.28 ns contiguous, ~9.6 ns scattered per candidate entity — index §2.2), not
  the text scan: string equality stops paying string prices.
- `prefix` — the prefix's dictionary range gives a contiguous ordinal range; the scan tests range
  membership, which is the numeric-range comparison.
- `in` — *k* resolves; the scan tests a sorted ordinal list, O(log k) per slot as today's `u32`
  set does.
- `contains` — two routes, chosen by a crossover, both built, and **both ending in the same
  ordinal scan**: they differ only in how the matching ordinal set is found, so whichever the
  crossover picks, the traversal is the same one and the choice is a price and nothing else.
  **Broad candidate**: a substring search over the dictionary's own key bytes, yielding matching
  ordinals. Because front coding elides shared prefixes, a substring can span an elided prefix, so
  **every key is decoded and searched — a per-key loop, not a flat byte stream** (review N5): at
  10⁹ unique keys the **decode alone measures 11.0–18.8 ns/key and the walk with its search
  15.4–26.6, so 15–27 s single-threaded**
  ([the dictionary campaign](../../probes/2026-08-13-keyword-dict/results.md),
  [the recovery](../../probes/2026-08-13-contains-recovery/results.md)) — above the 2–10 s this
  modelled, and measured at 2.4M rather than at 10⁹, so cache behaviour at 400× the size is not in
  it. Divided by cores, and milliseconds on repeat-heavy vocabularies; §11 item 4's harness owes
  the figure at scale, and **2–10 s must not be quoted as measured**.
  **Narrow candidate**: take the candidate's own ordinals, deduplicated and ascending, and decode
  each dictionary block holding one exactly once — **19.4–59.8 ns per candidate entity measured**,
  where a probe per entity cost 75–158 for the `restart_interval / 2` decodes it discarded. The
  crossover compares the candidate's cardinality against the dictionary's size — the principal's
  own quantity against a schema-derived one, the same admissible class as placement §3's route
  rule — and prices the narrow route at its **upper** bound rather than its typical cost, which
  errs towards the route whose cost the vocabulary caps. ⊘ That safety is now priced: on a
  contiguous quarter-corpus candidate over a unique column the rule takes the broad route at 41.1 ms
  where the narrow one costs 11.6. A rule that chose better would read the candidate's *distinct*
  ordinal count, which is §8.2's admissibility question and unruled. Substring keeps meaning
  substring, which is the identifier case's requirement and what `text` deliberately does not
  preserve (§4.4).

**An unresolved needle still scans.** A needle absent from a layer's dictionary is evaluated as an
ordinal no slot holds — the scan runs identically, and the empty result costs what any empty result
costs. Skipping the scan on a dictionary miss would make a value that exists somewhere in the
corpus distinguishable *in work* from one that does not, which is per-point-attributes §3.8's
requirement violated at the first place a reviewer would not look. With that rule, the scan route's
work remains a function of `(candidate, column)` alone, for every operator including `contains`
(whose dictionary walk reads every key whatever the needle).

**Per-term postings, where 0067 admits them, serve whole-value operators only — `eq` and `in`,
never `prefix` or `contains`** (review N1). Decision 0067's bound is the existence and coarse
frequency of a term the caller must already *possess*; a fragment operator answered from postings
would extend the channel to guessed fragments, which is beyond the ruling. The fragment operators
stay on the channel-free scan routes above, where §6.4 shows no budget pressure to move them; this
sentence is the tripwire for the optimiser that would.

**What retiring `utf8` costs, said out loud.** First, write-side weight: flat-column append was
O(bytes) with no structure; a keyword extent sorts and front-codes its batch at flush execution,
and the coalesce gains a dictionary merge with ordinal remap (§7). The hazards index §5 records
deleting — the shared dictionary, promotion, the resolver, the measured quadratic clone — stay
deleted, because identity is layer-scoped and no shared structure exists; but "no dictionary
anywhere" stops being true of the write side. Second, built and measured code is discarded: the
region search and the fuzzed byte predicates die with the flat column (the region-search idea
survives as the dictionary walk) — the SWAR machinery in the result-packing path is **not** among
the casualties; it serves fixed-width packing and survives. Decision 0048 licenses this; the
licence is used, not stretched. Third, the `contains` constant-profile shift above.

**What a keyword deliberately does not have**: a value set, a `listing`, a `/v1/categories`
counterpart, autocomplete, or any listing surface — index §1.1's refusals stand word for word. The
dictionary is never served; it is an index internal, and its ordinals never cross the trust
boundary.

### 4.4 Text

**Built, end to end.** A `text` field is prose: matched by what it says, not by its bytes. The
declaration, the analyser, all three producers of the index — the base build, the flush and the
compaction fold — and the `match` read route are implemented; ⊘ exact phrase is not (§4.5 remains
specification throughout), and a negation over a text column is **refused** rather than answered,
there being no per-item value for `none_of`'s presence half to subtract from.

**Storage is a token index; the values live in the record blob** (§3) whether or not the field is
indexed — `index = true` adds only the per-layer token dictionary and hybrid postings. **The
singleton encoding is worth about 4% here, not the 4.4× the keyword family measures.** That figure
is `id` and `doi`'s ([`keyword-and-list-storage`](../../probes/2026-08-12-keyword-and-list-storage/),
18.0 → 4.1 B/entity), where nearly every value has exactly one carrier; a prose vocabulary has a
long singleton tail under a head that dominates the bytes, and the shipped hybrid writer measures
22.58 B/entity against plain serialised Roaring's 23.55 on the same column and scale. Measured on real
titles at three scales ([`string-storage`](../../probes/2026-08-12-string-storage/) arm 3, whose accounting
[`text-index-bytes`](../../probes/2026-08-13-text-index-bytes/) then confirmed through the shipped
writers at 21.75–22.97 across three scales — the model was conservative by up to 8%): the
index is **22.58 B/entity on disk against the flat column's 83.6** — smaller than the column it
replaces,
stable across a 9.6× scale range, because a head token's posting densifies as a tail token's
spreads and the two cancel. With the blob record and its addressing beside it, **~59 GB at 10⁹
against 83.6 GB flat** — the compressed value bytes (~31 GB) plus the blob's own offsets, has-row
bitmap and directory (~4.4 GB; review B5's correction of an earlier ~55 GB that omitted the
addressing) plus the index; the ~1.4× win stands.

⊘ **Two limits on that sizing, and the second is not a scale caveat.** The 10⁹ figure is a linear
extrapolation of a per-entity cost measured to 2.4M; §11 gates promotion on extending it, and no
ruling below depends on its exact value, only its sign. And **every per-entity figure here is
title-shaped**: an arXiv abstract is ~13× a title's bytes and its index measures **158.43 B/entity,
7× a title's** ([`text-index-bytes`](../../probes/2026-08-13-text-index-bytes/)). The *ratio* to the
flat column improves with length — 6.47× against 3.56×, because a longer document repeats more head
tokens — but the absolute cost does not, so a corpus of abstracts sizes at ~158 GB of index at 10⁹
rather than ~23. **The index scales with prose length, not with entity count alone.**

**An analyser is a named, versioned pipeline, and a `text` column declares which one it uses**
([decision 0070](../decisions/0070-analysers-are-named-and-declared-per-column.md), amending this
section's original "one pipeline … with no per-column configuration"). One pipeline cannot be right
for a column of abstracts and a column of stack traces at once: identifiers split on case and
punctuation boundaries that prose must not, and prose wants folding that an identifier must not, so
the choice belongs to the column. The declaration carries the *name*; the manifest records the full
identity the build resolved it to, **per column**, and changing it rebuilds that column's index and
nothing else. ⊘ Analysers are built-in variants selected by name, **not plugins** — a loaded one
would make the token stream a deployment variable and demote the golden vectors from pinning the
analyser to pinning only a default, where determinism is load-bearing for I9 and for §7's
fold-merge argument.

**`unicode` is the one that ships**, and it is language-agnostic: NFKC
normalisation, full Unicode case folding, then UAX #29 word segmentation with dictionary-backed
segmentation for the scripts that need it — Chinese, Japanese, Thai, Lao, Khmer, Burmese.
Supporting a range of languages is a requirement of this design, not an aspiration, and it is what
rules out the obvious tokeniser: split-on-non-alphanumerics is Latin-only, and produces garbage
for every script without inter-word spaces.

**The pipeline is reused, not rebuilt: icu4x** (`icu_normalizer`, `icu_casemap`,
`icu_segmenter`) — Unicode's own pure-Rust implementation, no JVM (decision 0021), its data
versioned against a pinned CLDR/Unicode release recorded in the manifest. The segmenter chooses
its method per script run, so one analyser serves mixed-script corpora with nothing declared.
`icu_normalizer` is already in the dependency tree transitively (via `idna`); promoting icu4x to a
direct dependency of an indexing crate is stated here rather than hidden — the `memchr` precedent.
`lindera` (MeCab-style morphology, 50 MB+ dictionaries) is the named escalation if Japanese
segmentation quality ever demands it. Stemming, stopwords, diacritic folding and synonyms are all
deliberately absent **from `unicode`** (§9): each is language-dependent (ö ≠ o in German and
Swedish), each is a conformance surface, and a wrong default corrupts recall silently. Under 0070
that is a statement about this analyser rather than about analysers, which is what makes a future
stemming or identifier pipeline an addition rather than a contradiction — a new name, with its own
golden vectors, declared by the columns that want it. The index format does not change when an
analyser does; only the token stream.

**Measured coverage, and the gap that is quality rather than coverage.** Surveyed across
twenty-one scripts through the shipped pipeline: every space-separated script returns exactly its
source word count — Arabic, Hebrew, Devanagari, Bengali, Tamil, Telugu, Korean, Vietnamese,
Amharic, Georgian, Armenian, Sinhala, Turkish — and every no-space script is segmented, all six
of the ones named above included. ⊘ What is imperfect is segmentation *quality* in the no-space
scripts: Japanese `はとても` splits as `はと`/`て`/`も` and Thai `มาก` as `มา`/`ก`, so a query for
the mis-split word does not find the document. That is a recall shortfall on word-internal
queries rather than a coverage hole, it is per-script (a library that fixes Japanese fixes only
Japanese), and it is what 0070's named-analyser shape exists to let a deployment answer without a
format change.

**Whole-engine adoption was considered and declined on cost, not on Appendix D.** A text filter
only narrows `M_sel`, so the access-layer prohibition does not even arise; what decides it is that
the index half of text is mostly machinery Tessera already has or specifies — the front-coded
dictionary, Roaring postings, the extent/coalesce/fold lifecycle — while an adopted engine
(tantivy is the serious candidate) brings its own docid space needing per-query mapping, its own
segment lifecycle beside ours, non-Roaring postings, and corpus-global BM25, which §4.5 forbids.
Integration exceeds implementation. Its tokenizer API is an interface, not a capability; icu4x is
the capability.

**Conformance keeps the fixture-input relation, and the analyser is one implementation with two
accesses** (review B7). The oracle's expected `match` results are derived from the fixture's own
values — the strictly stronger construction §3 records — passed through the *same* analyser, which
the oracle reaches by invoking the Rust tokeniser (a `tessera tokenise` debug verb, on the
harness's existing drive-the-CLI precedent) rather than reimplementing it: PyICU wraps ICU4C,
whose segmentation can diverge from icu4x's. The pipeline itself is pinned by golden known-answer
vectors per script family — the same role the `tessera_id` vectors play, though there the oracle
re-derives independently; here independence lives in the vectors, not a second implementation. The
tokeniser's version is part of the manifest; changing it is a rebuild, exactly as changing a
category width is.

**Operators: `match`, its m-of-n form, and `phrase`.** All built. `match` is *every named token appears in the
field*, evaluated as an intersection of per-token postings inside the candidate;
`minimum_should_match` (ES's own name, semantics intact) relaxes it to *at least m of n*, evaluated
as a counting union — no statistics, no new storage. **The wire carries the query text, not
tokens**, and the engine analyses it with the column's *own* analyser, resolved from the identity
the manifest recorded when the index was built: a query segmented by one pipeline against an index
segmented by another matches on precisely the strings where they differ, with no error anywhere,
and analysing at the wire would put that choice in a second place free to drift. The candidate is
applied **per token as each posting is read** rather than once at the end, so nothing derived from a
corpus-wide set is ever held unmasked (I2), and an unresolved token contributes an empty posting
rather than short-circuiting — under m-of-n it must still consume its place in the count, or
`match` of three tokens with `minimum = 2` would silently become a two-token question. The timing
that leaves is decision 0067's accepted channel and Appendix C's **C25**, which lands with this
route as that ruling required. Over a multi-valued field (§5) `match` is **field-scoped, not
element-scoped**: tokens may match in different elements; element-scoped conjunction is a
positions question and arrives with §4.5's payload sidecar or not at all. `any_of` of single-token
matches gives disjunction through the existing tree (0062).

**What text deliberately does not do**: `eq` (declare a `keyword` for exactness — equality over
blob-resident prose would need the record at scan cadence, which §3 forbids); `contains` (byte
substring over prose is the trigram-postings design, refused with its measured security shape in
[`2026-08-09-text-contains-acceleration.md`](../evidence/memos/2026-08-09-text-contains-acceleration.md)
§5); corpus-global relevance, ever (§4.5); and any listing or suggestion surface (index §1.1).

### 4.5 Scoring and exact phrase

**Exact phrase is built** — v1, verify-against-the-record, as specified below. ⊘ **Scoring is
unbuilt and unscheduled beyond what §13 stages**, and the two are specified together because they
hang off one upgrade and one rule.

**The rule that makes scoring safe: every statistic a score reads is a function of
`(M_auth, query)` — score as if the visible corpus were the whole corpus.** Corpus-global inverse
document frequency is Appendix D's demonstrated channel — rank shifts computed from corpus-wide
statistics reveal the content of unreadable documents — and that is the half of §8.3's *filter, do
not rank* that is load-bearing. Mask-local document frequency is one `and_cardinality` per query
term against the candidate — the measured-cheap class — and a score so computed is inside I2 by
construction. On promotion this design owes §8.3 a sharpened statement: *no corpus-global
statistics* is the rule; mask-local ordering applied after intersection is admissible, and is
§8.2's own threshold-then-top-k discipline (ruled — decision 0069).

**The staging is quality-led, and it inverts the obvious order.** BM25's literature gains are
*ordering* gains — roughly doubling ranked-retrieval quality over coordinate matching, with IDF
carrying most of it on short queries and TF-saturation and length normalisation paying only on
verbose fields. A filter is a *set*, and a strict AND's set is already precise; weighting starts
buying quality exactly where the set loosens or must be cut to a cap. And a raw BM25 `min_score`
is a known-bad control — scores are query-relative and uncalibrated — where *matched m of n* is
calibrated by construction. So:

1. **Coverage is the filter primitive** — `match` + `minimum_should_match`, shipped with the
   family. No statistics, no storage.
2. **BM25 arrives as the selection order under the mark cap, not as a threshold.** When `M_sel`
   exceeds `k_max_marks`, the match layer is drawn best-first by mask-local BM25 instead of
   sampled relevance-blind — the first place scoring produces visible quality in this product, and
   a second is any future ranked drill-down list. It stays inside the rules: statistics are
   mask-local (I2), the score is deterministic so the served set remains a pure function of
   `(mask, corpus state, k, viewport)` (§10.4), selection is defined over the visible set (I7),
   top-k applies after intersection (§8.2), and every threshold and anchor stays on `M_auth`
   (surface §5.2) — the score orders only which of `M_sel`'s members fill the cap. It needs the
   payload sidecar below, a per-entity field-length byte or two, per-query masked DF, and an
   average length over `M_auth` — a fixed constant initially; the masked-sum version is the
   aggregate argument index §3 reserved bit-slicing for.
3. **A normalised-score threshold** (fraction of the maximum achievable score, so it is comparable
   across queries) follows only on demonstrated need.

**Exact phrase: one measured refutation, one v1 answer, one shared upgrade**
([`phrase-cost`](../../probes/2026-08-12-phrase-cost/)).

- **v1 is verify-against-the-record**, and is **built**: AND the phrase tokens' postings inside the
  candidate, then decompress the survivors' blob blocks and check adjacency there — zero storage,
  exact, and result-bound: 236–270 µs per block on selective phrases (§3 — the built reader's
  figure; the phrase probe's 169 µs was a decompression rate), unbounded on `"of the"`, which is
  the same accepted class as index §2.2's unselective predicates. Survivors are inside the composed
  candidate by construction (§6), so the verify reads nothing a filter result would not — and the
  implementation **checks that** before it touches a block rather than resting on the construction,
  since a survivor outside the candidate would be a block read on behalf of an item the principal
  may not see, which is worse than a wrong answer. Both sides of the adjacency test come from the
  same analyser, so a phrase is found exactly where the words the index holds are adjacent; word
  order and repetition survive on both sides, which is what distinguishes `phrase "the the"` from
  the deduplicated bag `match` resolves. A one-word phrase is a `match` and short-circuits the
  verify entirely.
- **Token-bigram terms are refuted**, and recorded so the idea is not re-derived: the pair
  vocabulary explodes (3.8M distinct over 2.4M titles, ~65% singletons at every scale measured),
  costing 61.7 B/entity on titles — **2.7× the 22.6 B/entity unigram index it would sit beside,
  and 3.4–5.7× the positional alternative below** (61.7 against 10.8 on titles; 500 against 145 on
  abstracts) — handing back the entire storage win over the flat column.
- **Positional payloads are the upgrade, shared with scoring.** Measured 10.8 B/entity on titles
  and 145 on abstracts (exact delta-varints, roughly a byte per token occurrence), and positions
  subsume term frequency — a term's TF is its position count — so the scored `match` and the exact
  phrase want the *same* bytes. The real price is mechanism, not storage: a Roaring bitmap carries
  no payload. The shape that preserves the posting-is-a-bitmap rule is ⊘ a **rank-aligned payload
  sidecar** per term — entry *k* belongs to the posting's *k*-th entity — so every existing
  consumer (match, aggregates, the candidate intersection) reads the bitmap untouched, while
  phrase and scoring walk survivors' ranks sequentially. **Its lifecycle is stated for both
  passes, not only the fold** (review N8): at a coalesce, a merged posting's rank sequence is its
  inputs' concatenation — valid because the merge refuses interleaving — so per-term payload
  arrays concatenate in merge order, and they count toward the pass's per-column input cap; at
  the fold they are rebuilt with the postings. Bought once, serving both; not scheduled until a
  need is demonstrated, and TF-only (10.4 B/entity titles, 80 abstracts) is its cheap half if
  scoring arrives first.

---

## 5. Multi-valued fields

⊘ **Specification throughout. `multi = true` is refused at the schema parse today, in every
family**, and the whole of this section describes what lifting that refusal would mean rather than
what a corpus can declare (§13 item 4; epic #87). It is written in the present tense below because
it is a design, and the reader should hold that every claim in it is a claim about the design.

**`multi = true` would be admissible in every family, and never with `render = true`.** Decision
0039's fence is unchanged and this design does not approach it: no projection, derived value or
summary of a list earns a hot column, and the parse refusal for `render` + `multi` names 0039 —
that half is built and permanent. Everything else lifts.

**An unindexed multi field needs no addressing at all**: it is a length-prefixed list in the
entity's blob row (§3). Everything below concerns indexed lists.

**Addressing is CSR above presence, uniformly.** A present entity's values occupy
`offsets[k]..offsets[k+1]` of the value array, where *k* is the entity's presence rank — one
`u32` offset per present entity beside the presence bitmap, in every family: native values for
numbers and datetimes, codes for categories, ordinals for keywords. This is index §2.1's
addressing generalised — the affine-rank traversal still merges candidate runs with presence runs;
what changes is that a run of entities becomes a run of values whose length is read rather than
assumed (placement §4). Measured, the CSR list scan costs 8–11 ns per candidate entity contiguous
— the string budget class, not the fixed-width one — and derived postings recover **up to ~50×,
with a slight loss in the one cheap cell** (contiguous 1% at a small dense vocabulary: 8.7 ns CSR
against 10.1 postings — review N6;
[`filter-placement`](../../probes/2026-08-12-filter-placement/) arm 3).

**Which lists get postings follows the family, not a special rule.** A category list derives
per-value postings exactly as the single-valued category does, under the same 0063 route split. A
keyword list derives per-term postings under decision 0067 (whole-value operators only, §4.3) —
and the re-run the memo asked for now exists: on **real** surnames the postings cost 20.1 B/entity
against CSR's 23.7 and won every timing cell, so the synthetic storage inversion that placement §4
rested on **does not survive real values**
([`keyword-and-list-storage`](../../probes/2026-08-12-keyword-and-list-storage/) §3). Placement
§4's "a string list gets no derived postings" rule is thereby retired in its grounding and its
effect. A number or datetime list has no vocabulary and derives nothing; its operators are the CSR
scan. A multi text field is a list of prose values whose tokens union into the same postings; its
record keeps element boundaries in the blob row, and `match` over it is field-scoped (§4.4).

**Semantics, stated before they are discovered** (placement §4):

- `all_of: [{c: {eq: a}}, {c: {eq: b}}]` changes meaning from vacuously empty to satisfiable. The
  request language does not change.
- `none_of` reads unchanged under decision 0066: *carries a value in this column, and none of
  these matches it* — `present ∩ candidate ∖ matched`. Presence is what the list's presence bitmap
  already says; positivity is preserved, and every "degrades safely under I12" argument keeps its
  sign.
- A range or `contains` over a list matches an entity iff **any** element matches. `all_of` of
  leaves expresses conjunction across elements; nothing expresses "every element matches", and
  nothing is asked to.
- **An empty list is absence**: the entity occupies no slot and no presence bit. It is not the
  empty string, which stays refused — an unset field and a client bug produce the same empty
  string, while an empty list has no competing spelling, and refusing it would make every entity
  with no tags unloadable.

**The write side is the bulk of the cost** (placement §4): extents carry offsets, the coalesce
merges them, the fold rebuilds them, and index §5.2's exclusion of lists from the merge lifts only
when that addressing lands. §7 specifies it.

---

## 6. Evaluation routes

Every route below resolves to an entity-space bitmap — or a row-space set bounded by the request's
own domain, admitted by decision 0068 — composed under 0062's tree, one crossing per request
(placement §2.2). Two rules bind every route in this section, stated once here because each has a
route it would be convenient to elide on. **The candidate is the composed verdict, never a raw
fragment and never bare `M_auth`** (review N2): suppressions touch no attribute artefact (§7), so
postings, dictionaries, ordinal columns and the blob all still contain suppressed entities, and a
route evaluated under anything less than the composed verdict silently resurrects one — surface
§5.1's sentence, which the row-bounded route of §6.3 and the phrase verify of §4.5 are as bound by
as any scan. And **the route is a function of the declaration and the request's shape** —
including, where a crossover is named, quantities the caller could compute for themselves — never
of a statistic about what the principal's data contains (§8.2).

### 6.1 Entity space: the scans and the postings

The masked scan serves every family as §4 specifies: fixed-width constants for numbers, datetimes,
categories and keyword ordinals; CSR constants for lists; the dictionary walk or per-candidate
probe for keyword `contains`. The postings serve categories under 0063 and **text**, whose
token postings three producers emit — the base build, each flush, and the fold that merges them.
⊘ **Keyword term postings remain specified and unbuilt**: 0067 admits them for whole-value
operators only (§4.3), and no producer emits one, so a keyword `eq` is answered by the dictionary
resolve and the ordinal scan. Per layer, unioned, disjoint
by I9 — unchanged from index §5.

### 6.2 Row space: the render column is filterable

A column with `render = true` is filterable **over the request's own rows**, against the
hot column in `columns.arrow`, producing `FilterRows::Viewport { rows, domain }` — a type that
exists, is consumed by `EffectiveMask::with_filter`, and is exact over its domain (placement §2).
**Every fixed-width family, and absence is what took the longest.** A category reads its absence
from the code its vocabulary reserves; a number, a datetime and a bool read theirs from decision
0064's presence bitmap beside the hot column, which is why they joined this route an epic later
than categories did (§2). A string is never rendered — the hot column is a fixed-width slot per
row, which is what makes it cheap enough to sit on the per-mark path — so `keyword` and `text` are
filterable in entity space alone.

**Over a category's codes the built route costs 0.22–0.46 ns per viewport row** — invariant in
corpus size, mask shape and coverage across 2.4M, 25M and 10⁸, and across viewports from 3×10⁵ rows
to a whole slice
([the epic-1 measurements](../evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md), [the campaign's follow-up](../../probes/2026-08-12-epic1-measurements/results.md); *measured*). A 343,391-row viewport at 10⁸ costs 0.13–0.16 ms.

⊘ **Every constant in this section is a 1- or 2-byte category column's, and none may be carried
onto a rendered number.** The route is built for all twelve fixed widths; it has been measured on
`u8` and `u16` codes alone, and the memo that took the constants says so in terms. At 10⁹ an `i64`
column is 8 GB against a `u8`'s 1 GB, and the one width comparison available — `u16` within a few
percent of `u8`, in both directions — establishes only that the scan is not bandwidth-bound at one
and two bytes on this machine. Decision 0064's presence bitmap, which is what let numbers,
datetimes and bools join the route at all, has never been timed either; its cost lands per run and
per segment rather than per row, so a sweep that varies rows while holding the run count still
cannot see it. `performance-suite.md` §3.3 owns the arm that closes both gaps.

**That constant is the loop's shape, not the column's, and it was found by measuring.** As first
built the scan resolved the segment and matched both the code width and the predicate *per row*,
which measured 2.5–3.4 ns — 4.8× the standalone probe's 0.48–0.73 and, tellingly, **insensitive to
the code width**, which a loop bound by moving one or two bytes per row could not be. Deciding the
width and the predicate once per contiguous run leaves a monomorphic compare over a slice and
recovers 6.5–10.9× (⊘ the A/B's *before* column was not saved; the same pre-hoist code measured in the earlier campaign gives 7.2–13.1×, so the published ratio is the conservative one — `probes/2026-08-12-epic1-measurements/`). The lesson generalises past this route: in a per-row loop over the hot column,
an enum matched inside the loop costs more than the comparison it guards.

A **fixed floor of tens of microseconds** survives the change — bitmap setup, and rayon fan-out on
the parallel path — so below roughly 10⁵ rows in the domain the per-row figure rises and the
parallel path is slower than the serial one. That is where the constant above stops holding: a
139,920-row viewport measures 0.62–0.66 ns per row and a 3,504-row one 15.3–15.9, which is the
floor divided by the rows, not a second constant. It is a fraction of a millisecond and inside §6.4's
budget, but it is the shape a per-keystroke filter over a small viewport takes.

**The probe's 7–1,269× advantage over the entity route is refuted for a category, and the reason
matters more than the number.** The probe timed the entity side as a per-entity value-column
scan; the built engine answers an indexed category from its **derived postings** (0063) in 20–54 µs
at 10⁸ on the selective values and 147–249 µs on the broad ones, which no scan can approach. Paired per value at a viewport the row route measures
0.09–0.27× the entity route's cost once the loop above is hoisted — it now wins at every viewport
shape measured — while **in the coarse-zoom cell the entity route is still 8–45× faster**
([the epic-1 measurements](../evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md), [the campaign's follow-up](../../probes/2026-08-12-epic1-measurements/results.md); *measured*). So the row
route's justification is **not** speed against an indexed column: it is that a rendered column is
filterable *at all* with no entity-space copy, and that it needs nothing on the write side,
because flush, merge and the fold already carry the scalar tail. Where a column affords both
routes, §6.2's route rule decides between them, and it is calibrated against these figures rather
than the probe's — see the note at its statement.

**No entity-space copy is stored for a rendered number or datetime.** This is the store-once rule,
scoped by §4.2's category exemption to the families it can safely reach (review B1): the hot
column serves the viewport route above, and the coarse-zoom cell — where the view is the corpus
and the row-space route degenerates to a whole-slice scan — is served by that scan, **measured
21–29 ms at 10⁸ single-threaded and 3.2–5.3 ms on twelve cores** (both columns and both values, at
the same 1- and 2-byte widths the caveat above scopes; [the campaign's follow-up](../../probes/2026-08-12-epic1-measurements/results.md)) — inside the 100 ms interaction target
without the sweep's parallelism rather than only with it — and **modelled 0.21–0.29 s serial /
32–53 ms parallel at 10⁹** on a constant flat within 10% from 25M to 10⁸, running inside the tile
sweep's existing
parallelism. At 10⁹ that is over the 100 ms target serially and inside the O(1 s) budget without
help; the sweep's parallelism brings it under the target as well. Before the per-run hoist the same
cell modelled 2.8–3.0 s serial and was inside O(1 s) *only* with that parallelism — which is the
state the alternative was weighed against, and the alternative is a second copy of every rendered
column. Placement §1's coarse-zoom result shows *neither* route dominating that cell. A future non-viewport filter
surface that needs entity space for a rendered number derives the column then — a build pass, not
a format change — rather than every deployment paying for the possibility now (decision 0048's
shape).

Two bounds inherited from placement §2.1, whose third — the membership question — is what §4.2's
exemption answers: the route is per slice; and a rendered **number** joined it with 0064's presence
bitmap, which is what lets the row scan tell an absence from the type's zero — without it the hot
column cannot express absence at all (§4.1, §2). The composition rule
when a tree names both kinds: evaluate the entity-space sub-tree, cross it once by surface §4's
measured rule, evaluate the row-space leaves over the crossing domain, combine in row space
(placement §2.2).

The route rule between the two, where a column affords both: **row space while
`rows_in_ranges ≤ |M_auth|`, entity space past it** — both quantities the caller could compute,
the same class of rule as surface §4's crossover (placement §3).

**Measurement supports the rule's shape, having first appeared to undermine it** ([the epic-1 measurements](../evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md), and the
campaign's follow-up). Against a category's derived postings the row route measures 0.09–0.27× the
entity route's cost at every viewport shape measured, and 8–45× *worse* in the coarse-zoom cell —
so row space at a viewport and entity space past it is the right direction, which is what the rule
says. What the rule cannot see is selectivity, and the crossover it draws with the request's row
span is not the one the costs actually cross at; the residual error is bounded by the viewport's
own row span and measures tens of microseconds at the shapes tested. That is a calibration
question, and it is
[#100](https://github.com/jennis0/tessera-index/issues/100)'s — noting that a selectivity-aware
rule would have to read a statistic about the principal's data, which §8.2 forbids a route rule to
do. A column with *only* the row route — a rendered column with no entity-space copy, which is the
case the operand exists for — is unaffected either way, having no alternative to choose.

### 6.3 The aggregate surface, and the row-bounded string route

Coarse-zoom counts, densities and legends are the product, and I2 requires each computable from
inside `M_auth` alone. For every scanned family the scan serves them (that is what it is for); for
text only the postings can (§4.4), and they exist — a `match` narrows the candidate and every
aggregate is then computed over the narrowed set exactly as for any other family, from inside
`M_auth` throughout. ⊘ For keywords the postings would close the one scan cell over budget (0067)
and none exists (§6.1), so a keyword's broad `contains` is priced at the dictionary walk §6.4
carries. Nothing new is owed in
mechanism — a term lookup lands in the same `postings ∩ candidate` construction the category
accelerator already takes — but nothing produces the postings it would land in.

⊘ The **row-bounded string route** (memo §6) is the interactive complement, worth building whether
or not the postings land: a viewport's rows reach their entities through `row-entity.u32`
(decision 0065, built), and the string operand is evaluated over *those entities only* — **~14–33
ms** for a 300,000-row viewport at 10⁹, any principal (modelled from measured constants, the
~15 ns/row lookup included — review X4). It changes no bytes on disk, is exact over its domain,
composes as the row-space leaf above under §6's composed-verdict rule, and its work is a function
of the viewport and the column, never the value. It is an optimisation for the per-keystroke cell,
not a substitute for the index: it cannot serve a caller outside a viewport and degenerates at
zoom 0 (placement §2.1).

### 6.4 The budget, honestly

Worst-case single-threaded figures at 10⁹ against the 0.5–1 s filter budget (index §2.2's owner
ruling) and this design's 100 ms target. Measured constants; the 10⁹ multiplications are marked:

| operator × shape | route | cost at 10⁹ | meets |
|---|---|---|---|
| category `eq`, selective (⊘ **a keyword `eq` has no postings** — nothing emits them, so the ordinal-scan row below serves it — §6.1) | postings (0063; 0067's are unbuilt) | 0.13–49.5 ms *(measured — the **category** alone)* | 100 ms |
| text `match` / m-of-n, common tokens | postings | tens–hundreds of ms *(modelled; §11 gates it)* | 1 s |
| any viewport-bounded filter | row space (§6.2, §6.3) | ≲ 1–33 ms *(probe-measured / modelled — N4's caveat)* | 100 ms |
| number range, contiguous candidate | scan | ~250–280 ms *(measured)* | 1 s |
| keyword `eq`/`prefix`, contiguous, no postings | ordinal scan | ~250–280 ms *(modelled from measured constant)* | 1 s |
| fixed-width scan, scattered 25% principal | scan | ~2.4 s *(measured at 10⁸ ×10)* | ÷ cores, measured 7.4–8.5× on twelve |
| keyword `contains`, unique vocabulary, broad candidate | per-key dictionary walk + scan | walk-with-search measured 15.4–26.6 ns/key at 2.4M → ~16–26 s at 10⁹ *(**extrapolation refused** — the dictionary fits this machine's L3 at the measured size and not at 10⁸ — `performance-suite.md` §5); **1.5–71× slower than the `utf8` scan it replaced** ([the fence](../evidence/memos/2026-08-13-utf8-retirement-fence.md), [the recovery](../evidence/memos/2026-08-13-contains-recovery.md))* | ÷ cores — blocks decode independently |
| phrase verify, selective phrase | postings ∩ + blob reads | ~ms–100 ms *(236–270 µs/block measured through the built reader; count result-bound)* | 100 ms |
| phrase verify, common phrase | as above | unbounded — result-bound | known class |
| CSR list scan, broad candidate | scan | ~2.7–10 s *(measured constants ×10⁹)* | **postings instead** |
| unselective predicate (matches ≥25% of corpus) | any | 3.4–6 s *(measured; result-bound — index §2.2)* | known gap, unchanged |

The rows that miss 100 ms without postings are the rows decision 0067 buys; the rows that miss 1 s
single-threaded parallelise over their own axis and are the price of not storing a second copy.
The last row is index §2.2's known gap and this design neither widens nor closes it.

---

## 7. The write side

The lifecycle is the one the filter index already has — extent per flush, coalesce between folds,
fold rebuilds and blanks — extended to the new artefacts. Nothing below adds a retirement rule:
write-path §5.4's Rule S and Rule F remain the whole of the removal model, every derived structure
is rebuilt whole at the fold, and a suppression touches no attribute artefact, ever.

**Flush.** A flush writes, per indexed column, one extent holding whatever the family stores: the
value slice (plus CSR offsets where multi), and for keyword the extent's **own** sorted dictionary
with ordinals against it. A **text** extent is the shape without a value column at all: its own
sorted token dictionary, postings over that dictionary, and a presence bitmap — no per-entity slot,
there being many terms per entity and no single ordinal to hold. Presence is stored rather than
derived from the postings because prose analysing to no terms — an empty string, a line of
punctuation — carries a value and appears in no posting. A batch with no value for the column at
all publishes **no layer**, an empty one being a permanent per-query cost until the next fold. The
analyser and the
keyword sort-and-front-code run at **flush execution on the pool** (write-path §4.3) — never on
the serial group-commit section write-path §2.3 defines, whose latency both lanes share (review
N9). A flushed batch is bounded, and there is no shared dictionary to promote into — which is what
keeps the near-unique-string quadratic hazard (index §5's history) impossible rather than avoided.
The flush also writes a **record-blob extent**: the flushed entities' rows, in their own blocks
with their own has-row bitmap, offsets and directory. An extent's presence bitmap remains
mandatory for indexed columns; the file set stays a function of the schema (index §2.5's
property).

**A layer's index files are one atomic unit** (review B2): the manifest record for a keyword or
text layer names its values, presence, dictionary and postings *together*, because an extent's
ordinals are meaningful only against **that extent's own** dictionary — resolving them against any
other layer's is a recolouring with no symptom. The reader rule is absolute; the record shape is
what makes a layer's files swap atomically at coalesce.

**Coalesce.** ⊘ *Built for text (2026-08-14); a keyword column still waits for the fold, index §5.2
saying why the two differ.* Attribute extents remain the entity-space pass's fourth axis (index
§5.2 — built for the shipped families), and record-blob extents join it as a fifth under the same per-column policy
(the pseudo-column `record`), merging by concatenation — entity-ascending across extents by I9,
the pass's non-interleaving guard making that sound — with small blocks repacked toward the
256 KiB target as a streaming rewrite. For keyword and text the merge gains real work: merge the
windows' dictionaries (a linear merge of sorted key sets), remap each input's ordinals through the
merged dictionary, concatenate CSR runs with offsets rebased, and merge postings per term. **What
this pass shares with the shipped coalesce is the window selection and publication mechanics —
not the correctness argument** (review B2): the shipped guard (union-cardinality-equals-sum over
presence) is sufficient today because values ride through byte-preserved, and the authorisation
dictionary-extent merge it echoes is *ordinal-preserving by construction*, which this merge is
precisely not. The renumbering therefore carries its own content guard: the remap must be
monotone (both dictionaries are sorted), and the merge verifies `merged_dict[remap[i]] ==
input_dict[i]` for every input key — O(keys), checked before any ordinal is written — so an
off-by-one that would recolour a window's values refuses instead of publishing. Postings payload
sidecars, where they exist, concatenate per term in merge order and count toward the input cap
(§4.5). A text column's postings count toward the cap as well.

**Fold.** The attribute pass (index §6.2 — built for the shipped families) extends per family.
Families with a value column stream base plus extents in entity order and emit one new base —
dictionary rebuilt whole from the surviving values, ordinals renumbered against it, postings and
any payload sidecar rebuilt from the folded column. **Text has no value column, and its fold is a
postings merge, stated as such** (review B3): the fold merges the surviving layers' dictionaries
and postings and subtracts the blanked set `D₀` from every posting. That is postings derived from
postings — the construction the artefact-of-record rule exists to avoid — so it carries its own
equivalence argument: the layers partition the entities (I9), `D₀` is exact, and every layer's
postings were produced by the same versioned analyser over the same values, so the merged content
equals a fresh build's over the surviving entities; the writer normalisation index §6.2 already
requires makes the equality byte-level, and records §10's folded-against-layered differential is
the check that keeps the argument honest. The blob is rewritten without the blanked entities'
rows. Blanking stays *remove, emit no bytes*: a deleted entity's prose is physically absent from
the folded blob and its tokens from the merged postings — the retention asymmetry argument (index
§6), now covering the record too, which is precisely why the blob lives under `attrs/` and folds
with everything else rather than in a store the fold does not touch. Term identity's whole
lifecycle is layer-scoped: minted at flush, merged at coalesce, rebuilt at fold, never durable —
memo §6.1's condition, met by construction.

**What the new write-side work costs** (review B4 — every figure *modelled*, per decision 0013;
§11 item 7 measures them). A keyword or text coalesce is a streaming merge bounded by the window's
own bytes under the existing 256 MiB input cap — sub-second per window at memory-bandwidth-class
merge rates. A keyword fold at 10⁹ is the dictionary rebuild plus the ordinal and postings
rewrite, inside the same ~12 GB/column streaming envelope index §6.2 models for a `u32` column. A
**text** fold is the expensive one: the blob rewrite decompresses and recompresses ~76 GB of
title-shaped value bytes (measured 1.5–1.7 GB/s decompress; compression *assumed* 0.4–0.8 GB/s) —
order three to five minutes of CPU per column — plus the dictionary and postings merge, a
container-rate stream over ~24 GB of serialised postings, order a minute; an abstract-shaped
column is roughly ten times the volume, so tens of minutes. All of it runs on the fold's own
thread inside the nightly gated window (decisions 0056/0057), extending the fold's duration —
free under the slower-is-gentler ruling (compaction §6.1) — not its intensity; nothing here
changes when folds run.

**Ingest wire.** A keyword or text column arrives as `utf8`; a list column as a list of the
element type; a category list as a list of value keys. Validation stays strict and typed per
attrs §5. Null is absence; the empty string stays refused; the empty list is absence (§5).

**Files**, extending index §2.5's layout. The record blob is not a column, so its extents cannot
live in `attr_extents` (which is column-keyed); the manifest gains a `record_extents` list beside
it, and the base blob files are named and digested like every base artefact — **a base or extent
blob file that is missing, short, or fails its digest refuses at open** (review B5), never "those
entities have no record":

```
attrs/<column>/values.arrow        indexed numbers/datetimes/categories: native values; keyword: u32 ordinals
attrs/<column>/offsets.arrow       indexed multi only: CSR offsets above presence
attrs/<column>/presence.roaring    indexed columns except text — see below
attrs/<column>/dict.bin            keyword, text: the layer's front-coded sorted dictionary
attrs/<column>/postings.arrow      categories (built); keyword/text terms — hybrid singleton encoding
attrs/record/blocks.bin            the record blob: zstd blocks in entity order (§3)
attrs/record/hasrow.roaring        entities that have a blob row (§3's rank addressing)
attrs/record/directory.arrow       block directory and rank-indexed within-block offsets
attrs/*/extents/<flush_id>.*       one set per flush, the blob included; every file digested; absence refuses
```

`record` is reserved as a column name at parse (§2), so the namespace cannot collide with a
declaration (review N10).

---

## 8. Disclosure

The invariants' statements do not change. What this section carries is the one leak-register row
this design needs — now ruled — the properties that bound it, and the refusals that stay.

**The scan routes stay channel-free by construction.** Every scan's work — fixed-width, CSR,
ordinal, and the keyword dictionary walk — is a function of `(candidate, column)` and never of the
value sought, including the unresolved-needle rule of §4.3. A hidden value and a nonexistent one
are indistinguishable in outcome and in work, which is per-point-attributes §3.8 held structurally,
as index §2.2 holds it today.

**The term-postings route is not, and the trade is ruled: accepted**
([decision 0067](../decisions/0067-term-timing-is-accepted-for-text-and-keyword-postings.md);
owner, 2026-08-12). A per-term posting resolves corpus-wide and is then intersected; measured, a
hidden value with members costs **1.26 ms over the shipped readers at 10⁸** and **2.1 ms in probe
arm 9 at 10⁹**, against 0.000 ms for an absent one (Appendix C row C24 carries both figures with
that attribution). For a category, 0063 bounds the route to `public` listings. A keyword or text
term has no listing, so the postings route makes *this string exists somewhere in the corpus, with
coarsely this many carriers* distinguishable from *it does not* — a C4-shape row with C8-adjacent
content, the class the trigram design was refused over. It is accepted for text and for keyword
postings because for text there is no alternative route (the record is compressed, no scan exists,
and the aggregate surface requires the index), because every comparable engine carries the same
channel ambient and unregistered where here it is registered and bounded, and because what it
discloses is existence-plus-coarse-frequency of a term the caller must already possess — never
membership, never which items, never anything about `M_auth`. Two consequences sharpen the bound
rather than relax it: **the route serves whole-value operators only** (§4.3 — a fragment operator
on postings would extend the channel to guessed fragments, beyond the ruling), and **the Appendix
C row must state the two possession bounds separately** (review X2): for keyword the caller must
possess a whole identifier, while for text any common word qualifies, so the text arm is a
corpus-wide term-frequency oracle over the vocabulary — accepted knowingly, and the row must not
read tighter than that. The route stays fixed by declaration, identical for every principal.
Conditions of the ruling: the Appendix C row lands with the first implementation, and §11 item 1's
measurement populates its figures at a real token vocabulary.

**Scoring adds no row.** Every statistic is mask-local (§4.5's rule), so a score is computable
from inside `M_auth` — I2's own test — and its evaluation work runs over postings the accepted row
already covers. **Phrase verify adds no row**: it decompresses only survivors, which are inside
the composed candidate by construction (§6); its result-bound cost is index §2.2's accepted class.
**The blob adds one coarse surface to name rather than discover** (review X1): a drill-down
block's decompression time reflects the content of a positional run of entities, invisible
neighbours included — single-interaction, C4-shape, weak. It is registered as **C26** beside 0067's own C25, rather
than argued away, because the register is exhaustive only if new surfaces are named.

**What stays refused, verbatim**: no value listing for any non-category family, no prefix
autocomplete (index §1.1 — an index the server never exposes publishes nothing, and the refusal is
of a *surface*, not of the structure); pre-intersection cardinality structurally unreachable
(§8.2); every count an `and_cardinality` against the composed verdict (surface §5.3); range
summaries masked or data-independent (surface §5.3); `none_of` positive (0066); no corpus-global
relevance (§4.5). Decision 0039's fence stands unmoved (§5).

---

## 9. What this deliberately does not do

- **No corpus-global relevance statistics, ever** — the load-bearing half of §8.3, unchanged. No
  ranked list responses either: score order appears in exactly one place, the match layer's cap
  selection (§4.5), computed from mask-local statistics.
- **No analyser *configuration***: no stemming, no stopwords, no diacritic folding, no synonyms,
  no per-column language settings. Each is a conformance surface and a config surface bought before
  anyone asks; the first real need reopens §4.4 with a measurement in hand. What decision 0070
  changed is *selection*, not this — an analyser is named and declared per column, and a column
  records the identity that indexed it — but each analyser is a built-in pipeline with no knobs and
  there is no plugin route. A second one is a variant in the binary, with its own golden vectors
  and its own version.
- **No positional payloads in v1** — record-verify serves exact phrase; the rank-aligned sidecar
  is the one specified upgrade (§4.5), bought only on demonstrated need. No fuzzy matching, no
  regular expressions.
- **No trigram or substring index** — refused with measurements and a security shape (§4.4);
  keyword `contains` is the dictionary walk or the candidate-driven probe; text has no `contains`.
- **No cross-column search** ("search everything") — a leaf names a column (0062), which is what
  keeps the leak register enumerable.
- **No aggregation surface** beyond the masked counts the filter contract already yields.
- **No vector search** — the sidecar slot §10.3 names stays open; nothing here occupies or
  forecloses it. The record blob takes over §10.3's *per-interaction metadata* intention natively;
  §13's amendments list carries the wording change.

---

## 10. Conformance

The oracle keeps the fixture-input relation (§3, review B7): expected values and `match` results
derive from the fixture's own generation functions, with text passed through the linked analyser
by invoking the `tessera tokenise` verb. The blob is checked through the served surface, plus one
narrow artefact-level check the fixture cannot see — the addressing self-consistency B6's refusals
depend on: rank, offsets, block bounds, discriminants. The analyser is pinned by golden
known-answer vectors per script family, dictionary-segmented scripts included. Where scoring
lands, the oracle recomputes the mask-local statistics independently — DF as
`|posting ∩ candidate|` from its own relation — and asserts the served cap selection is the
deterministic best-first order.

The adversarial value catalogue (surface §9) gains: a value present in one layer's dictionary and
absent from another's; a prefix range empty in one layer and not the next; an ordinal-boundary
value (first and last of a dictionary); a token appearing only in a deleted-but-unfolded entity; a
list whose elements straddle a CSR run boundary; an entity whose only value is in a coalesced
extent; the **first and last entity of a blob block, and a row adjacent to an oversized block**
(review B6); a mixed-script value and a value in a dictionary-segmented script; a phrase whose
tokens all appear in a field but never adjacently; a phrase straddling two elements of a multi
field (must not match — adjacency does not cross elements); a **suppressed entity carried by a
posting, a dictionary and a blob row** (the §6 composed-verdict rule, asserted on the new routes);
and a needle absent from every dictionary (§4.3's sentinel-scan rule, asserted in *work* on the
scan route — the one place a work assertion is the test, because the rule exists for it).

Differential coverage extends index §9's construction unchanged: the routes must agree — postings
against scan, row-space against entity-space over the domain (`filter_routes_agree_over_the_domain`
already asserts the crossing's version of this), folded against layered — the last being what
keeps §7's text-fold equivalence argument honest.

## 11. Measurements owed

Items 1–3 and 7 are load-bearing, and **where they run moved with §12's dataset ruling**
(owner, 2026-08-12): implementation precedes the scale tiers, so items 2, 3 and 7 run inside or
immediately after the text epic — over the machinery it delivers — and gate the 25M and 10⁹
dataset tiers, the removal of §4.4's extrapolation ⊘, and the corpus-scale claims, **not the
epic's start**. Item 1 populates the ruled row. Items 4–6 are owed but do not block.

1. ~~**Work at vocabulary scale**~~ — **measured**
   ([`hidden-vs-absent`](../../probes/2026-08-14-hidden-vs-absent/results.md), 2026-08-14), and
   Appendix C's C25 now carries the figures. Hidden costs the absent arm **+22 to +230 ns** at one
   carrier and **+10.9 µs** at 158k, over 264,919 and 476,423 terms through
   `FilterColumns::resolve`. The
   item's own premise is the negative result: **vocabulary scale is not what drives it**. The
   absent arm is flat across a 1.8× vocabulary growth — a failed dictionary search grows
   logarithmically — and the separation is a function of the queried term's posting alone, so
   "≥250k terms" was the wrong axis to have asked for. What is still open is the same channel
   *over a network*, which is where the practical question was all along.
2. **The token index's scale trend**: B/entity at an order nearer 10⁹ than 2.4M (the standard
   dataset's 25M tier is the natural rung). The *sign* — index smaller than flat — is what ruling
   the text ruling (§12) rests on.
3. **A non-Latin corpus under the real segmenter**: every storage figure so far is English under
   the probe tokeniser; a CJK or Thai corpus is a different vocabulary shape entirely, and the
   language requirement makes it load-bearing rather than a curiosity.
4. **Keyword dictionary at scale**: front-coded B/key **with restarts** (review N3's decodable
   format), resolve latency, and the per-key `contains` walk constant (review N5) at ≥10⁸ keys —
   the [`dict-fst`](../../probes/2026-08-03-dict-fst/) harness measured the neighbouring
   structure at 1.17×10⁸.
5. **Retrieval quality under our analyser** — coverage-AND against m-of-n against mask-local BM25
   on a labelled collection (BEIR SciFact / TREC-COVID scale) — before the scoring stage of §4.5
   is built, so the cap-selection gain is measured rather than imported from the literature.
6. Residuals. Three of the four are discharged by
   [the epic-1 measurements](../evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md):
   the blob's mixed-row compression ratio (§3's assumption — measured 3.00× against a 2.54×
   control), the built row-space route's constants against the probe's (review N4 — measured, and
   the probe's ratio refuted), and the coarse-zoom row-space scan under the sweep's real
   parallelism (§6.2 — measured at 10⁸, modelled to 10⁹). **What remains owed is arm-3 list
   *timing* on real skew** (storage is settled), and the two gaps the same campaign opened: the
   route's constants at a width above two bytes, and decision 0064's presence bitmap, neither of
   which has ever been timed (§6.2; `performance-suite.md` §3.3 owns both arms).
7. **The write side at scale** (review B4): keyword and text coalesce and fold throughput —
   dictionary merge and remap rate, blob rewrite through zstd, postings merge at container rate —
   against §7's modelled figures, at the 25M tier and extrapolated with the same honesty the read
   side gets.

## 12. Rulings — all made

Ten rulings govern this design and none is open. The first four were made during drafting; the
six the review left open were ruled on r3's presentation (owner, 2026-08-12).

**Made during drafting (r2):**

- **The term-timing channel is accepted** for text and for keyword postings —
  [decision 0067](../decisions/0067-term-timing-is-accepted-for-text-and-keyword-postings.md),
  with its two conditions (the Appendix C row at first implementation; §11.1's figures), §4.3's
  whole-value-operators restriction, and §8's two-possession-bounds note on the row's wording.
- **The declaration is `type` / `render` / `index` / `multi`, with the three-home rule** (§2–§3):
  an unindexed, unrendered field lives only in the record blob, and `index` is the ES word,
  adopted on 0062's borrowing test.
- **Scoring is staged quality-led** (§4.5): coverage as the filter primitive, mask-local BM25 as
  the cap-selection order first, a normalised threshold only on need, and the Truman-consistent
  statistics rule governing all of it.
- **Store-once holds for rendered numbers and datetimes** (§6.2), **and categories are exempt by
  construction** (§4.2 — review B1's disposition, applied with the rest of the review's findings
  in one pass; Appendix R).

**Ruled 2026-08-12, closing the review's open set:**

1. **`keyword` replaces `utf8`** (§4.3) — **yes**, as specified: per-layer sorted dictionary plus
   ordinal column, both `contains` routes, the coalesce guard of §7.
2. **`text` is in scope** (§4.4) — **yes**, as specified: token index plus blob record,
   `match`/m-of-n on the icu4x analyser.
3. **The row-space operand and its route rule** (§6.2) — **yes**;
   [decision 0068](../decisions/0068-a-row-space-operand-bounded-by-the-requests-domain-is-admitted.md)
   records it, being an amendment to §8.2's contract shape.
4. **Multi-value lifts for every non-render placement** (§5) — **yes**: `multi = true` is
   admissible everywhere except rendered columns, with the list semantics as specified.
5. **§8.3's sharpened statement** (§4.5) — **yes**;
   [decision 0069](../decisions/0069-filter-do-not-rank-sharpens-to-no-corpus-global-statistics.md)
   records it, being an amendment to the architecture's rule.
6. **The standard dataset** — **yes, amended by the owner in the ruling**: the **2.4M tier builds
   now** on the fixed-width and category schema, as the development and conformance corpus; the
   **25M and 10⁹ tiers are not built on it** — they wait for keyword, text and multi-value to
   land and are then built once, full-schema, carrying §11's scale measurements with them.
   Building them earlier would mean rebuilding them, and nothing needs them meanwhile: the
   serving-scale benchmarks already run on the existing synthetic 10⁹ fixtures, and the real
   snapshot tops out at ~2.4M records so the larger tiers were always synthetic-expanded.
   Consequences: §13's order pulls text forward, and §11's items 2, 3 and 7 run inside the
   dataset stage rather than gating the epics.

## 13. Order, and the amendments owed

The order reflects §12's dataset ruling: implementation first, as one pass; the large dataset
tiers wait for the string families and are built once.

1. **The surface change** (§2) with the record blob (§3) and the render-column route (§6.2) — one
   epic: schema parse, `/v1/meta`, the blob store and the conformance oracle move together, and
   deferring the surface means shipping a rule (`render` implies filterable) only to delete it
   (memo §8). The **2.4M fixed-width-and-category bundle** builds here as the development corpus.
2. **The keyword family** (§4.3) — replaces `utf8` wholesale; the dictionary and ordinal column,
   both `contains` routes, coalesce and fold merges with their guard (§7). Its postings can land
   behind it without a format change (they are derived).
3. **Text** (§4.4), pulled ahead of multi-value per §12's dataset ruling — the analyser (icu4x,
   golden vectors, the `tessera tokenise` verb), the token index, `match` and m-of-n,
   phrase-by-verify. Runs beside item 4 where the seams allow: the two touch largely disjoint
   machinery (analyser and postings against CSR addressing and the write side).
4. **Multi-value** (§5) with the row-bounded string route (§6.3) — the write-side epic placement
   §6 already names, now across all families. Keyword lists (`authors`) need items 2 and 4 both.
5. **The dataset stage**: the full-schema 2.4M rebuild (`title`, `authors`, `abstract` joining on
   the new families), §11's items 2, 3 and 7 measured over it and the machinery above, then the
   **25M and 10⁹ tiers, built once**.
6. **The scoring tail** (§4.5) — the payload sidecar and cap selection, after §11 item 5 says
   what it buys.

Nothing above changes an invariant's statement. I2 and I7 are argued at §4.5, §6.3 and §8; I12's
arguments all keep their sign through §5's positivity; I9 is what makes every layered union
disjoint; and the two removal rules are untouched at §7. The two architecture amendments are ruled — decisions 0068 (§8.2's second operand kind)
and 0069 (§8.3's sharpening) — and land in the amendments pass below.

**Amendments this design owes elsewhere on promotion** (review B8). Each is one edit at the named
site, and an unlisted falsified claim is a spec contradiction someone later "fixes" in the wrong
direction — so the ones an epic has already falsified are made as it lands rather than held for
promotion, and each bullet says where it stands. **The architecture's three are the whole of what
is still owed**; everything below them is made or is waiting on machinery that does not exist yet.

- **architecture §8.3** — **made**: the text paragraph names `keyword` and `text` with their
  operators, records that `utf8` is retired and refused, and carries text's two ⊘ (no exact phrase,
  no negation). ⊘ **§8.2's** ruled second operand kind (decision 0068) and ruled sharpening
  (decision 0069) are still **owed**.
- **⊘ architecture §10.3** — the routing-rule sentence gains the three-home rule and `index`; the
  "single adopted store" intention is superseded for per-interaction metadata by the record blob
  (vectors stay). **Owed.**
- **architecture Appendix C** — **made**: 0067's row is **C25**, carrying its measured figures
  ([`hidden-vs-absent`](../../probes/2026-08-14-hidden-vs-absent/results.md), §11 item 1) and both
  possession bounds stated separately, the text arm's being the near-vacuous one review X2 required
  it not read tighter than; the blob drill-down timing note (review X1) is **C26**. ⊘ **Appendix A**
  — the new artefacts joining the sizing tables — is still **owed**.
- **per-point-attributes §2, §4** — **made**: the `used_for` surface and the example schema are
  rewritten to §2's declaration, and the refusal list carries the `render` reason that is true of
  an ordinal. The `multi = true` refusal itself stays until §13 item 4 lifts it.
- **filter-index §1, §2.2, §2.6, §5** — **made** with the keyword epic: the family table names the
  dictionary-and-ordinal pair, §2.2's per-candidate text table is replaced by what a string scan
  now costs, and "no dictionary, so no promotion and no resolver" is scoped to the *shared*
  structure it was always about. **§5.2 and §6.2's lists-excluded markers stay** until §5's
  addressing lands.
- **filter-surface §2, §6** — **made, and §2 needed no edit**: that file names no family by type,
  and `/v1/meta`'s `filter_operands` already carries the new families and decision 0068's rendered
  operands.
- **contracts §2.2, §2.4, §2.6, §3.2, §3.4** — **made** for the record blob and the keyword family:
  `arrow_type` names `keyword`, §2.4 and §2.6 carry `record_extents` and the narrowed render tail,
  and §3.2's operators-by-family paragraph follows. **The ingest wire for lists stays owed** with
  §5's addressing.

---

## Appendix R — review trail

**2026-08-14 (r8) — the text family is built, and the corpus said otherwise in eight places.** A
three-lens adversarial review over the implementation found the design's status claims trailing it
badly: §4.4's own header called the read route, the flush extent and the fold unbuilt; §1's family
table read "⊘ unbuilt"; §6.1 said "text has no route at all" and §6.3 that it "has no aggregate
surface today"; and outside this document `architecture.md` §8.3 still named the retired `utf8`
family, `filter-index.md` §2 said a schema naming `text` was refused, and `contracts.md` omitted
`text` from `arrow_type`'s enumerated set while the build wrote it — so a second reader
implementing the contract would have refused every manifest this build produces. All corrected
here and there. Three claims were **wrong rather than stale** and are now stated: the 4.4× singleton
figure quoted in §4.4 is a *keyword* measurement (`id`, `doi` — near-unique vocabularies) and is
worth about 4% on prose; §7's flush paragraph gave a text extent "ordinals against it", which it
cannot have, having no per-entity slot; and §7's file list promised a `presence.roaring` the text
base does not write. §9's "no analyser configuration" was superseded by decision 0070 and now
distinguishes *selection* from *configuration*. §5 is marked ⊘ throughout — it was written in the
present tense over machinery the parse refuses. Appendix C gained **C26** for the blob drill-down
timing note review X1 named, and **C25** its measured figures and the second possession bound
review X2 required.

**2026-08-13 (r7) — analysers are named and declared per column** (owner ruling,
[decision 0070](../decisions/0070-analysers-are-named-and-declared-per-column.md)). §4.4's "one
pipeline … with no per-column configuration" is replaced: an analyser is a named, versioned
pipeline, a `text` column declares which one, and the manifest records the resolved identity per
column. `unicode` is the one that ships and its absent transforms — stemming, stopwords, diacritic
folding, synonyms — become statements about *it* rather than about analysers, which is what makes a
future identifier or stemming pipeline an addition rather than a contradiction. ⊘ Not plugins:
built-in variants, because a loaded analyser would demote the golden vectors from pinning the
analyser to pinning a default, and determinism is load-bearing for I9 and §7's merge. §4.4 also
gains the measured coverage picture — no coverage hole across twenty-one scripts, and a quality
shortfall in the no-space ones that is per-script and is what the named shape lets a deployment
answer.

**2026-08-13 (r6) — the keyword family is built, and §4.3's `contains` band is corrected to the
shipped one.** The dictionary, the ordinal column, both `contains` routes, the coalesce content
guard and the fold are implemented; `utf8` is retired as a declared type and its flat column is
deleted. The correction is what two follow-up campaigns and an adversarial review found: the
retirement fence's arms hoisted substring searchers the shipped routes built per key and per
candidate entity, and its best cell used a bench-local ordinal bitset — so r5's **1.5–71×**
described the tree plus three changes, where the tree itself was **2.5–144×**. All three have since
landed (`KeyMatcher`; the narrow route's deduplicated block walk; the domain-sized ordinal table),
so the recorded band is now the shipped one rather than an aspiration.

**The third of them is a disclosure fix.** The ordinal test was a binary search over the matching
ordinals, whose count is a corpus-wide property of the needle against the vocabulary — 1.41 ns per
candidate slot at five matching keys against 27.72 at 399,554, under a traversal that never varied
and a work harness that therefore could not see it. §8's claim that a scan's work is a function of
`(candidate, column)` alone was true of the dictionary walk and false of the scan after it; §4.3
and §8 now state the table that makes it true of both.

What remains open behind this revision is the crossover: it prices the narrow route at an upper
bound now well above its typical cost, taking the broad route where the narrow one is 3.5× cheaper,
and choosing better means reading the candidate's distinct ordinal count — an §8.2 admissibility
question the owner has not ruled on
([`contains-recovery`](../evidence/memos/2026-08-13-contains-recovery.md), which also costs two
further levers and names the one — needle-dependent pruning of the walk — that needs a ruling
rather than a patch).

**2026-08-12 (r5) — epic 1 is built, and §4.2's exemption narrows to its readers** (owner). The
declaration surface, the record blob through its whole lifecycle, drill-down's assembly from the
three homes and §6.2's row-space route are implemented; the ⊘ markers on those move. The narrowing
is what building it found: r3 stated the category exemption as the family's, and the build grants
the entity-space floor to an `index`ed or `per_viewer` category only — so a `public` category with
neither flag had no home at all and its values were dropped silently. §4.2 now states the floor as
its readers' and §3's rule — a field is blob-resident exactly when it has no other home — as the
one both placement passes ask. No mechanism moved; the exemption's argument is unchanged where its
premise holds.

**2026-08-12 (r4) — the six open rulings are made** (owner): keyword, text, the row-space operand
([decision 0068](../decisions/0068-a-row-space-operand-bounded-by-the-requests-domain-is-admitted.md)),
multi-value, the §8.3 sharpening
([decision 0069](../decisions/0069-filter-do-not-rank-sharpens-to-no-corpus-global-statistics.md)),
and the standard dataset — the last **amended in the ruling**: the 2.4M tier builds now, the 25M
and 10⁹ tiers wait for the string families and are built once, full-schema. §13's order pulls text
ahead of multi-value accordingly, and §11's scale gates move inside the dataset stage. Nothing
else changed; no section's mechanism moved.

**2026-08-12 (r3) — adversarially reviewed once, three lenses; every finding dispositioned in one
pass, all applied as the memo recommended** (owner, 2026-08-12;
[the review memo](../evidence/memos/2026-08-12-records-and-search-review.md) carries the findings
and the failed attacks). The verdict: security argument sound, read-side cost argument sound with
corrections, seams not survivable as drafted. What changed: **categories are exempt from
store-once by construction** — their entity-space structures are what the vocabulary machinery
runs on, and r2 had silently dropped placement §2.1's membership bound (B1); **the keyword
coalesce carries its own content guard and atomic layer record**, the "same shape as the authz
merge" claim being false in the load-bearing respect (B2); **the text fold is a postings merge
with an equivalence argument**, not a rebuild from a column text does not have (B3); **the write
side is priced**, modelled, with §11 item 7 owed (B4); **the blob's addressing, oversize rule,
manifest home and fail-closed read are specified** — has-row rank, whole-row blocks, `record_extents`,
bounds checks and an entity discriminant — and its storage total corrected to ~59 GB (B5, B6);
**the oracle keeps the fixture-input relation** (strictly stronger than the artefact-reading claim
r2 made) with one narrow addressing check added (B7); and **the owed-amendments enumeration
exists** (B8). Non-blocking: the whole-value-operators tripwire 0067 depends on (N1); the
composed-verdict rule stated bindingly over the new routes (N2); the dictionary figures re-scoped
to the decodable format (N3); the row-space constants re-marked probe-only (N4); the `contains`
walk re-modelled per-key (N5); the list-postings claim corrected to "up to ~50×, one losing cell"
(N6); the blob-versus-dictionary note restated per column shape (N7); the payload sidecar's
coalesce behaviour stated (N8); the analyser moved to flush execution (N9); `record` reserved
(N10); the blob timing note and the two possession bounds registered for Appendix C (X1, X2); the
rendered-number regression named with its restoration path (X3); and five figure attributions
corrected (X4), including the planner-caught error that the SWAR packing machinery survives the
`utf8` retirement. Nothing was rejected on verification; no disposition changed the design's
shape, so no re-review is triggered under the process's own rule.

**2026-08-12 (r2) — reshaped in an owner exchange, same day.** Five directions: `searchable` was
nearly vacuous and became the three-home rule with the key renamed **`index`**; the term-timing
channel was accepted (decision 0067); the analyser was respecified for the language requirement
(icu4x, dictionary segmentation, reuse over rebuild); scoring entered staged and quality-led
under the Truman-consistent statistics rule; and exact phrase was priced by a probe run for the
question ([`phrase-cost`](../../probes/2026-08-12-phrase-cost/)) — bigram terms refuted,
positional payloads shared with scoring's TF, verify-against-record as v1. Multi-text pinned
field-scoped; §4.3 gained the `contains` two-route rule and the `utf8` retirement costs.

**2026-08-12 (r1) — drafted**, from the day's two memos and three probe campaigns, one run for
the draft ([`keyword-and-list-storage`](../../probes/2026-08-12-keyword-and-list-storage/)): it
settled the keyword layout (dictionary + ordinals) and closed the memo §4 authors question (the
synthetic CSR-versus-postings storage inversion does not survive real values).
