# 0113 — Ordinals are removed; the key is a view's only address

**Date:** 2026-08-31 · **Status:** Settled (owner ruling, 2026-08-31) · **Supersedes:** [0110](0110-the-ordinal-gap-is-accepted.md)

## Context

A view of a group carried two addresses: its caller-chosen key, and an ordinal assigned at
creation, served on `/v1/meta`, stored in the roster record and the drop record, tracked by a
per-group high-water, and usable as `<group>:#<n>` wherever a view id went — including a pinned
filter leaf, `sentiment@#3`.

The ordinal existed to give a client an ordering it could walk without interpreting a key. It also
gave a principal a number: under the view gate a gate-failed view is omitted from the roster while
the ordinals stay monotone, so a reader of `0, 1, 3` learns a view exists at `#2` that they may not
reach. Decision 0110 accepted that as a register row (C27).

## The decision

**Ordinal addressing is removed** (owner ruling). A view in a group is addressed `<group>:<key>`
and by nothing else. A caller who wants a numeric ordering mints numeric keys.

**The ordinal goes entirely, not only as an address** (architect's consequence of the ruling):

- `/v1/meta` serves no `ordinal` field. A group's views are listed in **creation order**, which is
  roster-record order — records are appended and never rewritten, so the order survives with no
  number stored anywhere.
- The roster record in the segments manifest and in `MANIFEST.json` drops the field, `ViewDrop`
  drops it, and the per-group high-water is deleted with them.
- The pinned leaf keeps `<column>@<key>` and loses `<column>@#<n>`.
- `#` is no longer reserved out of a key by name; the column-name charset refuses it with every
  other punctuation, and an id in the old form names a key nobody declared — the ordinary
  unknown-view `404`.

**Key tombstones are unchanged.** A dropped key is refused for ever, and it always was the key
rather than the number that would repoint a bookmark or a client cache ([0029](0029-view-key.md)).

> **Superseded in part by [0115](0115-a-dropped-view-key-is-reusable.md)** (2026-09-01, owner
> ruling). The burn is withdrawn: a dropped key may be created again, at a fresh internal
> incarnation that keeps its predecessor's artifacts out. The 0029 citation above was an
> over-generalisation — 0029 names a cache coordinate that contains no view id — and the clause
> was never separately ruled. The rest of this decision stands: the key is still a view's only
> address, and the incarnation is not a second one.

**Appendix C's C27 (roster ordinal gaps) is deleted**, and this decision supersedes 0110. The
register enumerates channels that exist; with no position served, a gate-filtered roster is a
shorter list and nothing else, and a principal reading it cannot count what was withheld. This is
the rare direction — a leak closed by removing the machinery that carried it, rather than accepted.

## Consequences

- A client offering previous-and-next walks `groups[..].views`, which is what the TypeScript
  client already did; nothing there sorted by the ordinal.
- No version number moves (owner direction: frozen until launch). A bundle or WAL written before
  this change carries a field nothing reads and is recreated rather than migrated
  ([0048](0048-no-deployments-exist-so-delete-rather-than-support.md)).
- `views.md` r16, `contracts.md` r58 and `architecture.md` r54 carry the change; the conformance
  suite's roster check asserts the **absence** of the field rather than an ascending sequence.
