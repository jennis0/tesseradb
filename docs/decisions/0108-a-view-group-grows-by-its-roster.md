# 0108 — A view group grows by its roster, and a roster record is immutable

**Date:** 2026-08-30 · **Status:** Settled (owner rulings, 2026-08-30)

## The decision

A **view group** is a set of views sharing every setting — projection, extent, point visibility,
gate — differing by a caller-chosen **key** and typed per-view metadata. A view of a group is
`<group>:<key>`, or `<group>:#<ordinal>` — the ordinal monotone at creation, never reused, an
alias; the key required. `:`, `#` and `@` are reserved out of names and keys.

**Plain views are constant: none is added after the build.** Growth at a running service is what
groups are for. A group's view is created ahead of the rows that name it by
`PUT /control/views/{group}/{key}` carrying the roster record; the record is **immutable** — a
wrong gate or metadata is a drop and a recreate under a new key, because an updatable gate is a
narrowing that does not bite live sessions. The roster's durable home is the **segments
manifest**, carried forward for ever like `layer_tombstones`; the WAL carries only the replay
records.

The declaration takes two roster forms: `[[view_group.view]]` — one view per block, one points
file per view — or `[view_group.views]` — the roster as a table beside one points file with a
`fields.view` discriminator. The roster decides where points come from; declaring both is
refused. A group may take another group's views (`members = "…"`), chains refused.

**A session's visible-view set is fixed at authorise**: a view created afterwards is a 404 to
that session until re-authorisation. Creation is rare, tokens expire, and the alternatives cost
the work-indistinguishability the gate's closure rests on.

**Dropping a view deletes no entity.** An entity left in no view keeps its label, attributes and
memberships; `delete_dangling = true` is sugar that submits ordinary deletions through the
overlay, serialised against ingest, retiring at the fold under Rule F — never a second
retirement route.

`views.md` §3, §4, §6 (r6) is the design; the review trail is its Appendix R.
