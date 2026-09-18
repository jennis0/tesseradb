# The capability map

**Status:** Descriptive, 2026-09-11. Records what is built, as read from code. Decides nothing.
Every claim in this memo is read from code; where a design document and the code disagree, the
memo records both and says which was read.

## The finding

Tessera does not have one type system. It has several, and they do not share capabilities.

Point attributes are one system, with five families: number, datetime, category, keyword and
text. A sixth shape, a list of any of these, is refused everywhere it could apply. Alongside the
point attributes sit further systems, each with its own type and its own narrower surface: access
labels, geometry, view-group metadata, artifact supplied content, artifact shape, layer membership
keys, vocabulary values, and computed artifact content.

Point attributes carry most of the system's capability. They filter, render, drill down, scope per
view group, highlight, and, for category and for a plain numeric column of the right width, become
a layer of artifacts. Every other system does less. Access labels are not data; they are the mask,
and nothing filters on them. View-group metadata filters nothing. Artifact supplied content holds
strings only, and the one filter over it is a substring search on a single route. Vocabulary values
are reached only through the category column they belong to, never on their own.

A reader who assumes one uniform type system gets specific things wrong.
An artifact cannot be filtered by anything it carries beyond a text substring, even though a point
can be filtered on five families of attribute. A keyword column has no enumeration and no
typeahead, though category, its close relative, has both. Text cannot be negated, though every
other point-attribute family can be. Each of these is a boundary of the narrower system that holds
the value, read from the code that enforces it, and none of them is stated where a reader building
against the filter surface would first look.

## Matrix A: point attribute types against capability

Two rules cut across every row of this matrix. A column's home follows its declared flags:
`render` puts it in the row tail served with the points frame, `index` puts it in its family's
entity-space structure, and a column with neither goes to the record blob. Whether a column can be
filtered follows the same flags: `is_filterable = index || (render && reaches_hot_column)`. Second,
nothing is filterable before its flush: a value is durable at the acknowledgement of the write and
visible only at the next flush tick, 90 seconds by default.

Legend: Y built, N not built/refused, — not applicable.

| capability | number (bool, u8..u64, i8..i64, f32, f64) | datetime (timestamp_us) | category | keyword | text | list of any |
|---|---|---|---|---|---|---|
| declare at build | Y | Y | Y (needs a `[[vocabulary]]`) | Y | Y | N refused at parse |
| load at build | Y (Parquet only) | Y, microseconds only; Date32/Date64 and other units refused | Y, as value keys never codes; source must be Arrow `Utf8` | Y, `Utf8` only | Y, `Utf8` only | — |
| declare at a running service | Y | Y | Y, the vocabulary must already exist | Y | Y | — no such field on the route |
| load at ingest on the creating row | Y | Y | Y, key | Y | Y | — |
| fill on an existing entity (`/control/values`) | Y | Y | Y | Y | Y entity-scoped; N for a group-scoped cell that has flushed | — |
| edit an existing value | N — 409 | N | N | N | N | — |
| clear a value | N — no route, any type | N | N | N | N | — |
| render: travel with the points frame | Y | Y | Y, as its code | N refused at the schema | N refused at the schema | N refused (decision 0039) |
| return at drill-down | Y | Y | Y, as its key | Y | Y (record blob) | N — the reader answers absence |
| scope per view group | Y | Y | Y | Y | Y, but `index = true` is required and no drill-down returns it | — |
| filter operators | `eq` `in` `range` | `eq` `in` `range` | `eq` `in` | `eq` `in` `prefix` `contains` | `match` (+ `minimum_should_match`) `phrase` | — |
| negate under `none_of` | Y | Y | Y | Y | **N — 422** | — |
| highlight | Y, same grammar | Y | Y | Y | Y | — |
| enumerate its values | N | N | Y `/v1/categories` | N, by design | N | — |
| typeahead | N | N | Y `/v1/categories/{col}/suggest` | N | N | — |
| per-value count | N | N | Y, in the suggest answer only | N | N | — |
| become a layer of artifacts | Y only `u8`/`u16`/`u32` with `index` | N | Y with `index` | N | N | — |
| sort, rank or score on | N | N | N | N | N | — |

## Matrix B: the other type systems

Point attributes are the rich system; this table is the other eight. Read it for what is absent:
most of these systems can be filtered through only one fixed route, most cannot be edited at all,
and removal, where it exists, always removes the whole thing rather than one value inside it.

| system | the types | attaches to | filterable by | editable | removable |
|---|---|---|---|---|---|
| access labels | string terms, a list per point; `public` reserved at term 0 | points; an artifact takes its layer's gate | no — they are the mask, not data | no — delete and re-ingest | with the entity |
| geometry | `x`/`y`, or `lon`/`lat` under a projection, or `morton`+`residual`; one per view | points | the `region` leaf: bbox, circle, ellipse, polygon, in `view` or `wgs84` space | no | with the entity |
| view-group metadata | the attribute types minus `utf8`; one value per view, on the roster | views | nothing — it filters nothing by design | no route | no — no view-group or plain-view delete |
| artifact supplied content | `text`, `polygon`, `extent`, `point` at the build; the type also knows `circle` and `ellipse`; the control plane validates no word set | artifacts | only `/v1/artifacts/browse`'s `q`, a case-insensitive substring over the key or the first text content | fill-once; a differing restatement is 409 | withdrawn only by emptying its generating set, and it does not come back |
| artifact shape | bbox, circle, ellipse, polygon (WKB at the build, WKT inline) | artifacts | spatial membership; `region` by published artifact | fill-once | with the artifact |
| layer membership keys | `Utf8` or any integer width, scalar or list; null and `-1` mean no artifact | points, naming artifacts | the `member_of` leaf | join only | never — a membership cannot shrink |
| vocabulary values | key, optional title, and a `u8`/`u16`/`u32` code; code 0 is the absent sentinel | categories | through the category | title upserts; key and code are immutable | retired to `reserved`, never removed, and only at a build |
| computed artifact content | `centroid`, `box`, `hull` | artifacts | no | recomputed per viewer from current membership | — |

## Matrix C: subjects against create, fill, edit and delete

One rule governs the whole write plane. Applying a record is monotone: a part that is absent is
filled, a part that is present and identical to what is stored is accepted with no effect, and a
part that is present and different from what is stored is refused. There is no edit anywhere in
the system. Editing any value means deleting the entity and re-ingesting it, which mints a new
`tessera_id` and keeps the `external_id` (decision 0047).

| subject | create | fill | edit | delete |
|---|---|---|---|---|
| point / entity | build and `POST /control/ingest` | `POST /control/values` | no | `POST /control/changes`: delete, suppress, unsuppress |
| artifact | build and `PUT /control/layers/{name}/artifacts` | `PATCH` the same route | no | only by deleting its entity through `/control/changes`; no artifact route |
| layer | build and `PUT /control/layers` | — | no, 409 on a differing identity | `DELETE /control/layers/{name}`; the name is tombstoned for ever |
| attribute column | build and `PUT /control/attributes` (`render` refused at the running service) | — | no | **no route** |
| vocabulary | build and `PUT /control/vocabularies/{name}` | `PATCH .../values` appends; a title upserts | key and code no | **no route** |
| view of a group | build and `PUT /control/views/{group}/{key}` | — | no | `DELETE` the same path; the key is reusable, the incarnation dies |
| plain view | build and `PUT /control/views/{name}` | — | no | **no route** — a rebuild |
| view group | build and `PUT /control/view_groups/{name}` | — | no | **no route** |
| level | build, fixed at layer registration | — | no | **no route** — drop the layer |

Reversibility: `unsuppress` is the only reversal in the system. A deletion, a layer drop, a
withdrawn content and a retired vocabulary code are all irreversible. An entity id, an artifact
ordinal and a layer name are never reused; a view key is.

## What would surprise a reader

1. **No edit and no clear, for any value of any type.** An attribute cell is write-once: absent is
   filled, identical is accepted, and a different value is refused with a 409 (`ingest.md` §1.4).
   No route clears a cell, for any type (`write.rs:10821`, `:10871`). Access
   labels follow the same rule: a second row naming a different label set is refused
   (`write.rs:11127`). The only correction is to delete the entity and re-ingest it (decision
   0047), which mints a new `tessera_id` and keeps the `external_id`. A caller expecting to patch
   one field in place must instead replace the whole entity.

2. **`text` cannot be negated, and `/v1/meta` does not say so.** `none_of` over a `text` column is
   a 422. A negation is `present ∖ matched`, and `text` owes no value column, so the engine holds no
   presence set to subtract from (`engine/filter.rs:3118`). ⊘ The base build writes no presence file
   for a text column either (issue #123), so the gap does not close at the extent that would
   otherwise carry one. The server's own code names the gap:
   "`none_of` is not universal over the columns published here, and nothing on this surface says
   so" (`viewer.rs:418-424`). A client that builds its filter UI from the published operand list
   finds out that `text` is exempt only when the request is refused.

3. **`multi` is refused everywhere, so no list-valued attribute exists.** `multi = true` is refused
   in every family at declaration (`config.rs:4651`); `render` with `multi` is refused permanently
   (decision 0039, `config.rs:4640`); and `RecordValue::List` is refused at the writer as well as
   at parse (`filter/record.rs:301`). A list is undeclarable and unwritable both. An attribute
   meant to hold several values needs a different shape, because no route reaches one end to end.

4. **Per-artifact access labels parse, build, and withhold every artifact on the layer.**
   Declaring `artifact_visibility = { field = "<column>" }` parses at the build and marks the
   layer as carrying its own labels (`config.rs:5144`). The verdict path then asks for the
   artifact's own term to test, and every production caller passes `own_terms: None`
   (`browse.rs:445`, `viewport.rs:5344`, `:6101`, `:6409`), because neither the build nor the
   control plane ever supplies one (`types/layer.rs:204`). The layer builds without error and
   withholds every one of its artifacts from every principal. This is C27's open item in the leak
   register.

5. **Artifacts carry strings and geometry, and nothing else, so they cannot be filtered by what
   they hold.** Supplied content is `Vec<String>` whatever its declared type (`control.rs:4690`);
   there is no numeric, category, keyword or date attribute on an artifact anywhere
   (`viewport.rs:5340`, `:6098`, `:6403`). The only filter over content is
   `/v1/artifacts/browse`'s `q`, a case-insensitive substring over the key or the first supplied
   text value. A question such as which artifacts hold a population over some threshold has no
   route to answer it.

6. **Keyword values are never enumerable, by design.** `/v1/categories/{column}` and its
   `/suggest` sibling serve category columns only. Keyword has no equivalent: no value list, no
   typeahead, no per-value count. A filter UI offering autocomplete on a category column has
   nothing to call for the keyword column beside it.

7. **No aggregate over a data column, and no sort or score anywhere.** No route returns a minimum,
   maximum, sum or histogram of a data column. No route accepts a caller-specified sort: orders
   are fixed, and browse orders by count descending. Filtering answers whether something matches,
   never how well (decision 0069, "filter do not rank"). A question such as the top ten rows by
   some value, or the average of a value within a mask, cannot be answered by the server; a client
   must fetch the matched rows and compute it itself.

8. **Vocabulary codes are not carried across rebuilds.** `configuration.md` §1 states that a
   rebuild replays each value's recorded code. The build does not: `assign_codes` starts from an
   empty slate on every run, because nothing reads the previous manifest (`config.rs`, near
   `assign_codes`, line 4478). Reordering a bare `values` list therefore reassigns every value's
   code, and every stored row whose category is that vocabulary now carries a code that means a
   different key. Pinning a code explicitly, with `key = code`, is the only way to hold it still
   across a rebuild.

9. **A declaration that carries attributes cannot be built with no source for them.**
   `configuration.md` §2 describes declaring a schema and deferring its data, "declare now, write
   later," against an empty bundle. The build refuses an attribute with no `source`
   (`config.rs:2253`). The empty-bundle path in §2 does not work for a declaration that carries
   attributes; an operator following that document as written meets a build refusal instead of
   the empty bundle it describes.

10. **`region` and `member_of` are evaluated but not published on `/v1/meta`.** Both leaves work
    as filters, but neither appears in `filter_operands`; the published enum lists only the seven
    column operators (`viewer.rs:449`). A client finds `region` in the `selection` block and
    `member_of` in the `layers` array. The operand list that names every other filterable column
    omits both.

11. **Category postings cover the base build alone.** Postings are built once, at build time, and
    nothing since (`filter.rs:43`). A category `eq`/`in` filter is posting-routed over rows from
    the base build and ordinal-scanned over everything ingested since, until compaction rebuilds
    the postings. The answer is correct either way; what changes is the cost of reaching it, and
    nothing reports that the cost has changed.

## Where the documents and the code disagree

The rows below are documentation defects. One stands apart: the supplied content row is a
disagreement inside the code itself, between the build, the type definition, and the control
plane, and no document states any of the three positions.

| document | what it says | what the code does | file |
|---|---|---|---|
| `configuration.md` §1 | A rebuild replays each vocabulary value's recorded code | Codes are assigned from an empty slate on every build; reordering a bare `values` list reassigns every value's code | `crates/tessera-build/src/config.rs`, `assign_codes`, line 4478 |
| `configuration.md` §2 | A declaration can name attributes and defer their source against an empty bundle ("declare now, write later") | An attribute with no `source` refuses the build | `crates/tessera-build/src/config.rs:2253` |
| `wal.rs` comments on `VocabularyDeclare`, `ViewGroupCreate`, `PlainViewCreate` | "Not built yet: nothing writes this record" | All three are written; `unbuilt_track` returns `None` for every variant | `crates/tessera-lifecycle/src/wal.rs:505`, `:513`, `:524`; writers at `crates/tessera-engine/src/write.rs:2669`, `:2819`, `:2836`, `:13669` |
| `ingest.md` §8 | The order-of-work table does not mark T1, T2b, T5 or T6 built | §1.1 and §1.3 of the same document already record T2b, T5 and T6 as built | `ingest.md` §1.1, §1.3, §8 |
| `architecture.md` Appendix C, C32 and C33 | Both marked "⊘ Not built" | C32's highlight is built server-side; C33's browse verb, `POST /v1/artifacts/browse`, is a served route | `crates/tessera-server/src/viewer.rs:1067`; `crates/tessera-server/src/control.rs` route table |
| `highlight-and-hierarchy.md` Status line | "§5's client is not" built | The client carries highlight and filter verbs on every clause, including per artifact | `clients/ts/components/src/hierarchy.ts:450`, `:457` |
| `config/tests.rs:3789` comment | "The multi-view build is specified and not implemented" | The adjacent test asserts a group's views are materialised; `views.md` records it built; conformance carries `test_multiview_differential.py` | `crates/tessera-build/src/config/tests.rs:3789` |
| `config.rs:2105` comment | "Not yet enforced anywhere, there being no `/v1/categories` to filter" | `/v1/categories` exists and `Visibility::Derived` is enforced | `crates/tessera-engine/src/categories.rs:212`, `:505`; `crates/tessera-engine/src/filter.rs:1552` |
| Supplied content type (a three-way split inside the code; no single document states it) | `SuppliedContent::authored_shape_kind` recognises six words: `text`, `polygon`, `extent`, `point`, `circle`, `ellipse` | The build accepts four of the six (`text`, `polygon`, `extent`, `point`); `LayerDeclaration::validate` checks none of them, so `PUT /control/layers` accepts an arbitrary string as a content type | `crates/tessera-types/src/layer.rs:396`, `:1204`; `crates/tessera-build/src/config.rs:5909` |

## What this memo does not cover

Performance: no capability listed above is timed or costed here. The conformance suite's own
coverage: the module list behind this memo names which modules exist. It does not say which claim
above each one checks. The client beyond what was read for this memo: `filter.ts`'s range-only
numeric UI and `hierarchy.ts`'s per-clause verbs are the extent of what was examined, and nothing
about their correctness or completeness is claimed beyond that. Sharding is not examined here.
