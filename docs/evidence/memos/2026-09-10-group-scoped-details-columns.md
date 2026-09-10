# A group-scoped details-only column has nowhere to live, and the blob's format says it should

**Date:** 2026-09-10
**Status:** Provisional. A question and the evidence for it. Nothing here is ruled; the owner asked
for the case to be written up rather than patched.

## The question

An **entity-scoped** string column declared with neither `index` nor `render` is a details-only
field: stored, not searched, returned on drill-down. It is an ordinary thing to want — a display
name, a title, an identifier — and since 2026-09-10 it takes the record-blob extent route and costs
12.97 B/item rather than 44.28 ([`build-column-extents.md`](../../design/build-column-extents.md)).

A **group-scoped** one cannot be declared at all if it is `text`, and is accepted but unservable if
it is `keyword`. The owner asked why group scoping should make that difference. It should not, and
the blob's own format was built expecting it not to.

## What is refused today, and what is quietly allowed

`config.rs` refuses a group-scoped `text` column without `index = true`:

> Its terms are its only home — the record blob is one bundle-wide list with no slot for a column
> family (views §5) — so without the index the prose would be read and stored nowhere.

A group-scoped `keyword` in the same position is **not** refused. It writes a per-view
`values.arrow` and a dictionary, so it is stored — but nothing can read it back: a filter surface
wants `index`, and drill-down reads the blob, which has no slot for it. **Stored and unservable.**
The two families are treated differently for a reason that applies equally to both.

## Half the stated objection does not hold

The manifest's own note (`manifest.rs`, `Group::scoped_scalars`) gives two reasons a family has no
slot in `declared_scalars`:

> a scoped column placed there would take a slot in every row and a whole-corpus `attrs/<column>/`
> of its own, both absent for every entity.

**The first is not true of the record blob.** `record.rs` specifies a row as self-describing:

> `field := tag u16 LE | kind u8 | value` … A field's absence is its absence from the row; there is
> no null encoding and no per-field presence structure.

An entity outside the group's views omits the field and pays **zero bytes**. "A slot in every row"
describes a segment's fixed scalar tail, which is addressed positionally, not a blob row, which is
tagged. The objection is sound for the one and not for the other.

**The second stands and is separate.** A scoped column should not acquire a bundle-wide
`attrs/<column>/`; that is about the value column, not the blob, and nothing here proposes to change
it.

## The encoding it would need already exists

`record.rs` specifies kind 13:

> `13 list (elem_kind u8 | count u32 LE | count values, element encoding only)`
>
> The list encoding is specified now and populated in epic 3 … the decoder implements it so the
> format is real and pinned by test; `encode_row` refuses a list value until the multi surface
> lands, so no artefact can carry one early.

So the container is real, decoder-complete and tested. And the keying the owner proposed — the same
keying the views use — needs no key bytes at all: `scoped_scalars` is already "one entity-space
column **per view of the roster**", so a list whose *i*-th element is the *i*-th view's value is
positional against a roster the group already owns. Elements carry the value encoding only, which is
exactly what a homogeneous per-view list wants.

## What is actually in the way

1. **The multi surface has not landed.** `encode_row` refuses a list value, deliberately
   ([records §5](../../design/records-and-search.md), `multi = true` refused at schema parse in
   every family, epic #87). A per-view list is not the multi-value model — one is a value per view,
   the other several values per entity — but they share the encoding, and today the encoding is
   gated as a whole.
2. **The tag space.** A blob `tag` is a column's position in `declared_scalars`, and a family is
   deliberately outside that list. Admitting one means either putting families into it or giving the
   blob a second tag space. That is the real design decision.
3. **Nothing else.** The extent route a details-only entity-scoped column now takes would serve a
   scoped one the same way, once it has a tag to be written under.

## What the owner may want to rule

- Whether a group-scoped details-only column is a thing a corpus may declare at all. If not, the
  scoped `keyword` case should be refused as scoped `text` already is, on the same sentence — it is
  currently accepted, stored, and unreadable, which is the worst of the three outcomes.
- If it is, whether the blob carries it as a roster-positional list, and where its tag comes from.
- Whether that waits on the multi surface or is separable from it.

⊘ **Not measured.** No corpus in the ladder declares a group-scoped details-only column, so nothing
here has a figure attached; `multiview` declares a group-scoped `text` **with** an index, which is
the case that is allowed today.

⊘ **A stale citation, found on the way.** `record.rs`'s list paragraph cites "decision 0013's
marking discipline"; there is no `0013` file in [`docs/decisions/`](../../decisions/). Either the
decision was renumbered or the reference predates the directory's convention.
