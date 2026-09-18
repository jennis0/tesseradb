# The capability gaps

**Status:** Descriptive, 2026-09-11. Records the new capabilities the two memos beside it
surfaced, so that the consistency pass builds none of them and none is lost. Decides nothing and
schedules nothing; each item names what governs it and where it is already tracked.

**Reads with:** [`2026-09-11-capability-map.md`](2026-09-11-capability-map.md), whose §"What
would surprise a reader" is the source of §2 and §3, and
[`2026-09-11-duplication-and-consistency-pass.md`](2026-09-11-duplication-and-consistency-pass.md),
whose register is the source of §1. The line between the two memos: the pass removes a second
copy or a disagreement between copies; an item here makes the system do something it does not
do today.

## 1. Surfaced by the register

Two register rows are statements a build accepts and a running service refuses, where closing
the row means building the capability at the service.

| Item | Today | Governed by | Ruling |
|---|---|---|---|
| category-typed view metadata at a running service | refused at the group declaration (`view_declarations.rs:341`) and at the create (`roster.rs:269`); the build resolves a key against the vocabulary, minting on an open one and refusing on a closed one (`config.rs:3984`) | views §3.2; decision 0134 | decision 0140 C: the create route takes the key as the build does, the handler resolving and the write executor minting |
| authored `circle` and `ellipse` content, end to end | the type recognises the two words (`layer.rs:396`); the build refuses them (`config.rs:5909`); the control plane checks no word; nothing outside the build's membership shapes handles a `ShapeKind::Circle` or `::Ellipse`, so whether an authored one is drawn is unverified | polygon-membership §6.1 (h) | decision 0140 D fixes the closed set at six words on both doors; drawing them is this item |

One consequence of the pass is a small widening and is listed so it is not read as an accident:
under decision 0140 I a layer's gate becomes a list of labels, any one satisfying, as a view's
already is. A layer may then declare two labels where it could declare one. `configuration.md`
§4 gains the sentence.

## 2. Promised by the design and not built

Each row is a capability a normative document describes and the code lacks. The capability map
records the disagreement; the work is to build the capability, and the pass's T6 marks the
document "Not built yet" until it exists.

| Item | Design says | Code does | Tracked |
|---|---|---|---|
| vocabulary codes carried across rebuilds | configuration §1: a rebuild replays each value's recorded code | `assign_codes` starts from an empty slate; reordering a bare `values` list recodes every stored row (`config.rs:4478`) | not tracked |
| declare now, write later | configuration §2: a declaration may name attributes and defer their source against an empty bundle | an attribute with no `source` refuses the build (`config.rs:2253`) | not tracked |
| per-artifact access labels | configuration §4: `artifact_visibility = { field = … }` | parses, builds, and withholds every artifact on the layer from every principal, because no caller supplies the artifact's own term (`types/layer.rs:204`; `browse.rs:445`, `viewport.rs:5344`) | C27, architecture Appendix C |
| `none_of` over a `text` column | `none_of` is universal over the published operands | 422: a text column has no presence set to subtract from (`filter.rs:3118`); the base build writes no presence file for it | [#123](https://github.com/jennis0/tessera-index/issues/123) |
| `region` and `member_of` on `/v1/meta` | the operand list names every filterable column | both evaluate; neither is in `filter_operands` (`viewer.rs:449`) | not tracked |
| `render` on `PUT /control/attributes` | decision 0134: anything a build can declare, the service can | refused under decision 0136's amendment until the route can address a row | decision 0136 |
| the both-doors test at corpus scale | decision 0091: any input spelling can be driven both ways and the two bundles compared | the pass writes the declaration-rule half in `tessera-server/tests`; the corpus half does not exist | not tracked |

## 3. Absent by design

Listed so the gap is not rediscovered as a defect. Each is a decision, and reopening one is a
decision file.

| Item | Decision |
|---|---|
| no edit and no clear of any value; a correction is a delete and a re-ingest | 0047 |
| no enumeration, typeahead or per-value count for a `keyword` column | records-and-search §4.3 |
| no aggregate, sort, rank or score over a data column | 0069 |
| no list-valued attribute | 0039 for `render`; the route is [#87](https://github.com/jennis0/tessera-index/issues/87) |
| category postings cover the base build until a fold | filter-index §9; the cost is unreported, which is a measurement item and not a capability |
