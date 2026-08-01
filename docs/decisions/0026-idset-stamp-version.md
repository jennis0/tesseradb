# 0026 — Three concepts that shared the word "epoch" get three words

**Date:** 2026-08-01 · **Status:** Settled

## Context

Three unrelated concepts were all called "epoch", and the second-commonest form was the bare,
ambiguous one — sixty uses of "the epoch" with no qualifier:

- the counter that advances on **key rotation**, governing which `tessera_id` values are valid;
- the monotonic build markers that govern **when a deny may retire** — a deletion deny may leave
  the overlay only when no servable fragment predates it;
- a **table's version** in the slices design, `table = (slice, group, epoch)`.

The consequence is specific and it is the failure class this project has already caught twice. The
deny-retirement rule is stated in terms of "epoch". A reader who reaches for the key-rotation
counter gets a rule that retires denies on rotation, which re-exposes deleted items.

## Decision

| Concept | Word |
|---|---|
| Key rotation; which identifiers are valid | **idset** |
| Build markers governing deny retirement | **stamp** — fragment stamp, tombstone stamp, fold stamp, `min_live_stamp` |
| A table's version in the slices design | **version** |

"Generation" and "watermark" were both unavailable — the first is the engine's snapshot type, the
second the ingest marker.

**idset** names the set of identifiers a rotation replaces, which is what a consumer needs to reason
about: their cached identifiers belong to an idset, and a new idset means they are stale.
**stamp** is a monotonic marker with no time connotation, which is what the retirement counters are.
**version** matches the existing `segments_version` and `overlay_version` family, which is the same
kind of quantity.

## Cost, and why now

The idset rename touches published surface — the manifest schema, a `/v1/meta` field and an HTTP
status detail. Contracts §0.3 deviation 10 records that a deployment which has published an API must
bump rather than rename. Nothing is published, so this is free today and a breaking change later.

## Related

[0025](0025-rotation-is-a-session-invalidation-event.md) changes what the idset concept *does*, as
well as what it is called.
