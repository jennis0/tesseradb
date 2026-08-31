# 0114 — The item drill-down serves the satisfied labels only

**Date:** 2026-08-31 · **Status:** Settled (owner ruling, 2026-08-31)

## Context

`POST /v1/items/{tessera_id}` served an item's record and its caller-supplied external id, and
nothing about **why** the item was visible. A viewer holding several grants could see the item and
could not tell which of their grants admitted them to it — a question a detail panel is the natural
place to answer, and one the service could answer cheaply.

It could not answer it from what the bundle stored. Labels live term-major, as postings: the index
answers *which entities carry term `t`*, which is what a mask is built from, and reading one
entity's labels back meant a sweep of the whole dictionary. The same gap had already been accepted
elsewhere — `views.md` §4's join rule refuses a second view's row that names a *different* label
for an entity, and past that entity's own flush the comparison had nothing to compare against, so
the refusal was marked ⊘ and the batch was accepted (inert, a joining row carrying no descriptors,
but unreported).

Two shapes were on the table for what the drill-down would carry.

## The decision

**A. The satisfied terms only** — the intersection of the item's own term set with the session's
satisfied set, presented through the authorisation plugin. **Ruled.**

**B. The item's full label set.** **Declined.** A viewer must never learn a compartment they do not
hold. An item's full label set says that this document is also filed under something the reader has
no grant for — a fact about the corpus's labelling, not a filtered view of it, and exactly the
shape §4's **I2** forbids for a count. That the reader can see the item is not an argument for it:
visibility is granted by *one* satisfied term, and the others were never within their authority.

**Terms are not sensitive as bytes.** How a descriptor is spelled for a viewer is a plugin concern,
so the boundary gains a presentation function (`architecture.md` §6.1) — and for the only plugin
that exists, `builtin:passthrough`, the descriptors **are** the caller's own label strings, so its
implementation is the identity. That is its correct answer and not a fallback: there is no mapping
to look up, and serving the descriptors verbatim is what identity means.

## Why the intersection is structural rather than a filter

A filter can be forgotten; this one cannot be, because the serving path holds no name for a term
outside the grant. A session keeps the descriptors its own credential presented, **plus `public`**
— the one label every principal holds, added inside the trust boundary at authorise and so in
every session's satisfied set whatever the credential said. That map is the **only**
ordinal→descriptor route the request path has: the bundle carries no reverse dictionary, and at the
plugin's declared 2×10⁸ terms it would be gigabytes of one for a surface that can only ever name
terms already inside the session's own authority. So a bug in the intersection can drop a label the
viewer holds; it cannot invent one they do not.

**The one function that could is the plugin's, and its count is checked.** `present_terms` maps
descriptors to strings positionally, and a plugin returning *more* strings than it was handed would
put on the wire a label answering to no term this session satisfies — the disclosure this section
otherwise rules out, arriving through the seam the argument does not itself constrain. The
serving path refuses a count that does not match, in either direction.

## What it costs

An entity→term transpose in the bundle (`contracts.md` §2.4, `entities/terms/`): a has-row bitmap,
rank-indexed offsets and concatenated ordinals, written by the build, extended by each flush, and
rewritten minus `D₀` by the compaction fold. No ordinal is ever remapped — a dictionary ordinal is
a position in the `dict_extents` concatenation, which a coalesce preserves by construction and the
fold carries forward by hard link.

It is **server-side**, and the two surfaces that read it are not the same surface. The drill-down
reads the intersection. The write path's join arm reads the **full** set, because equality is its
whole question: an arm that compared only the terms the writer named would accept a batch that
dropped one. `views.md` §4's label arm is therefore exact past a flush, and its ⊘ is discharged.
The attribute half of that marker stays: an entity-scoped value's server-side home is the filter
column or the record blob, and reading it back to compare would be a second value oracle across
every declared family, which is a wider surface than one transpose and a pass the fold and the
coalesce would each owe.

## Register

**C30** in `architecture.md` Appendix C, accepted. The residual is which of a principal's own grants
admits them to an item they can already read in full. It says nothing about an item they cannot see
— a masked, suppressed or unknown identifier is the same `404`, body for body — and nothing about
the item's other labels, their number or their existence.

## Consequences

- `architecture.md` §6.1 gains `present_terms`, §7.4 states the rule, Appendix C gains C30 (r55).
- `contracts.md` §2.4 defines `entities/terms/` and §3.2 the `labels` array (r61).
- `records-and-search.md` records that the drill-down carries labels beside the record.
- `views.md` §4's label arm is exact; its attribute arm stays marked (r21).
- No `api_version` or `bundle_format` bump. The array is additive, the artefact is new, and
  [0048](0048-no-deployments-exist-so-delete-rather-than-support.md) has the bundles rebuilt.
