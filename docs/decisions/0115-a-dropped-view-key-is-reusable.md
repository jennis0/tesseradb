# 0115 — A dropped view key is reusable, and an incarnation keeps its predecessor out

**Date:** 2026-09-01 · **Status:** Settled (owner ruling, 2026-09-01) · **Amends:**
[0108](0108-a-view-group-grows-by-its-roster.md) · **Supersedes in part:**
[0113](0113-ordinals-are-removed-and-the-key-is-the-only-address.md)

## The ruling

**A view key dropped by `DELETE /control/views/{group}/{key}` may be created again.** The burn is
withdrawn: `PUT` on a previously dropped key is a `201`, and the `409` remains for a key that is
**live**. Drop and recreate inside one commit window is a supported sequence.

Two reasons, both the owner's:

- **A key is a name, not a system identity.** It is chosen by the caller, it addresses a view on
  the wire, and it means whatever the record under it says. Entity ids, term ids and a published
  identity are irreversible because a rerun cannot undo them; a view key is neither of those.
- **The correction workflow requires it.** A roster record is immutable ([0108](0108-a-view-group-grows-by-its-roster.md)):
  a wrong gate or wrong metadata is a drop and a recreate. That workflow is close to useless if the
  correction cannot be made under the name the operator's pipeline, dashboards and documentation
  already use — the remedy was "mint `2026-Q3-v2` and change everything that names the old one".

**A recreated key's contents are what the name now means.** No promise is made that a view id
addresses the same rows across a drop; the drop is the event that says otherwise, and it is one the
operator performed.

## What the burn rule was, and where it came from

`views.md` §3.4 said "a dropped key is never reused, because a recreated `2026-Q3` with different
contents would silently repoint every bookmark, every cached θ and every client cache keyed on the
view (decision 0029)", and the create verb answered `409` on a tombstone. `SegmentsManifest`
carried `view_tombstones`, published at every flush and carried forward for ever.

**It was never separately ruled.** It arrived with the roster work as a read-across from the layer
registry's `layer_tombstones`, and its citation was over-generalised in two steps:

- [0029](0029-view-key.md) names the composite **(mask, overlay version, slice, k, idset)** the
  "view key" and says a client keys its cache on it. That coordinate does not contain a view id at
  all, so it is not a decision about reusing a `<group>:<key>` name. What 0029 does establish is
  that a *cache keyed too loosely is a disclosure* — which is why the argument sounded load-bearing.
- The layer analogy does not carry either. A layer name travels in bookmarks, in artifact edges and
  in suppressions, so a name that once meant something must not come to mean something else. A view
  key addresses a row space and a roster record, and both go with the drop.

**0113 cited the burn as unchanged** ("Key tombstones are unchanged") while ruling the ordinal out.
That clause is superseded here; the rest of 0113 stands, and the key remains a view's only address.

## The hazard the burn was actually closing

Strip the client-cache argument and one real hazard is left, and it is internal.

A dropped view's artifacts do not disappear at the drop. Its segments stay in the live
side-manifest, its group-scoped columns stay in `attr_extents`, `text_extents` and
`scoped_columns`, its derived structures stay in `tile_index_extents`, `row_column_extents`,
`shape_rows_extents` and `shape_held_extents`, and its buffered rows stay in the WAL. All of them
are unreachable only because the view is no longer *declared* — reclamation by omission
(`views.md` §3.4), the fold's to complete.

The moment the key can be created again, "is this view still declared" stops separating them: the
recreated view is declared under the same id, in the same bundle directory, and a composition
resolving artifacts by view id alone would serve the predecessor's points under the new name. That
is a wrong answer wearing a legitimate state's clothes, with no symptom anywhere.

## The design: incarnations, internal only

**Every roster record carries an incarnation** — a monotone counter minted at create and recorded
in the `ViewCreate` WAL record, never re-derived at replay. A build-declared view is incarnation
`0`; a key first used at a build and later dropped comes back at 1 or above. The next value is one
above every incarnation the manifests and the log carry, live or dead, which is a seed a restart
can check rather than trust.

**It is on no wire.** `/v1/meta` is unchanged, no response carries it, and it is not part of a view
id. A principal cannot tell a recreated key from one created for the first time, and there is no
timing structure to read either: the create takes the same path it always did.

**Every artifact of a view carries the incarnation it was written under, or resolves through
something that does.** The choice is per class, and the rule is *what can the restart path verify
with no ordering argument*:

| Class | Choice | Why |
|---|---|---|
| `SegmentDescriptor` | **carries** | The points. A restart has the manifest and the roster and needs nothing about when either was written. |
| `AttrExtent`, `TextExtent` | **carries** (`Option`, present exactly when `view` is) | Carried forward for ever, so an extent outlives the drop that orphaned it. An entity-scoped column belongs to no view and carries neither field. |
| `ScopedColumn` | **carries** | Same list, same argument; it is what a restart recovers `scoped_scalars[..].views` from. |
| `TileIndexExtent`, `RowColumnExtent`, `ShapeRowsExtent`, `ShapeHeldExtent` | **carries** | Addressed by *row*, so one written over a dead incarnation's row space would label the recreated view's rows. |
| `ViewDescriptor` | **carries the live one** | The single resolution site: `Manifest::incarnation_of` and `is_live_incarnation` are what every filter asks. |
| `ViewData` (an opened row space) | **carries**, read off its segments | `Bundle::with_views` blanks a view whose data is a dead incarnation's, which is where the old "retain the declared views" test used to be enough. |
| WAL rows and buffer entries | **resolves**, through replay order | Replay is strictly ordered and `ViewDrop` occurs between the rows it kills and the rows the recreate takes. It discards the buffer's rows for that view exactly as the live path does. A stamp on `WalRow` would widen every row buffer in the commit window to record what the sequence already states. |
| `MembershipExtent`, `RecordExtent`, `EntityTermsExtent`, `LocatorExtent`, the overlay, the deny list | **neither** | Entity space, and incarnation-independent — the same reason `delete_dangling`'s deletions are ordinary deletions. |

**One artefact needed a path and not just a stamp.** A scoped column's *base* —
`attrs/<column>/<group>/<key>/values.arrow` and the three files beside it — is the only thing a
view owns at a fixed path: everything else it writes is named by a `seg_id` that is never reused.
A flush of a recreated key would therefore have written straight over its predecessor's base, and
that is not merely untidy. The live manifest still digests that file, so a crash between the write
and the next publication leaves a bundle whose digests refuse to open; and the dropped view's
column may still be memory-mapped, `filter_columns` being carried across a drop unchanged, so the
truncation is the same hazard the flush `seg_id` attempt counter exists to prevent. The directory
therefore carries the incarnation above the build's — `<key>@<n>` — through one derivation
(`tessera_store::scoped_column_rel`) that the writer, the digest pass and the opener all take.
`@` is reserved out of a view key, so the suffix cannot collide with one, and at
`DECLARED_INCARNATION` the path is exactly what every existing bundle already lays down.

**A drop expands to every id the key names.** A key is a view of the group that owns it *and* one
of every group sharing its views (`views.md` §3.3), so anything acting on "the views of this key"
takes `Manifest::view_ids_for_key`: `with_roster`'s death loop, the drop's buffer prune, its
`delete_dangling` probe, and replay's own prune (which is passed the expansion, `tessera-lifecycle`
holding no manifest). Three separate spellings of it is how one site comes to prune a single id and
leave the other's rows for the next incarnation to adopt — reachable from either end, since the
live path built its id from the group the *request* named while the record always carries the
owner.

**Fail-closed everywhere it is asked.** An artifact whose incarnation cannot be resolved — an
unknown view, a half-stamped entry — is omitted, never treated as live. Omitting a derived
structure costs a recomposition; adopting one serves another key's rows.

## Tombstones become bookkeeping

`SegmentsManifest.view_tombstones` is renamed **`dead_view_incarnations`**, a list of
`{group, key, incarnation}`. It is no longer a refusal — nothing measures a create against it — and
it is not silently repurposed under the old name: an entry now says *this incarnation is dead and
its artifacts are on disc until the fold reclaims them*. `RosterError::Tombstoned` is deleted with
it, and the create verb's `409` table loses its tombstone row.

`WAL_VERSION` moves to 18 and `ViewDrop` carries the incarnation that died — which is what makes
the record self-sufficient when rotation has reclaimed its own create. `bundle_format` stays at 4
and the new manifest fields are **required**, on the rule the surrounding fields already keep: an
absent incarnation would read as the build's, which is the one value a leftover artifact of a
dropped-and-recreated key could carry, so a `serde(default)` here would adopt exactly what the
field exists to keep out.

**`with_roster` applies the deaths before the creations.** Both lists are complete current state
rather than a diff, so with a drop and a recreate in one window the order is what decides whether
the recreate survives; the other order deletes the view the caller was just told it had.

## What is not changed

- **`delete_dangling` is untouched.** Its deletions are entity-space and incarnation-independent,
  they enter the overlay, and they retire at the fold that executes them (Rule F, write-path §5.4).
  It is still not a second retirement route.
- **The two removal rules are untouched.**
- **A roster record is still immutable** ([0108](0108-a-view-group-grows-by-its-roster.md)). The
  correction is still a drop and a recreate; what changed is that the recreate may reuse the name.
- **The key is still a view's only address** ([0113](0113-ordinals-are-removed-and-the-key-is-the-only-address.md)).
  The incarnation is not a second address and cannot be spelled on any request.
- **`LayerDrop` still tombstones a layer name**, and the difference from a view key is the point:
  a layer name travels in bookmarks, edges and suppressions.
- **`dead_view_incarnations` is the mint's only durable floor, and nothing prunes it today.** The
  seed is one above every incarnation the roster and the log carry; the live records alone do not
  bound it, because the highest incarnation a key ever had is exactly the one a drop moves into the
  dead list. So a reclaim that ever prunes an entry must leave a high-water behind it — on
  `entity_id_high_water`'s pattern — or the mint regresses at the next restart and a fresh
  incarnation is stamped with a number its predecessor's leftovers already carry. Pruning is not
  built, and this is the condition on building it.

## Evidence

`crates/tessera-types/src/view.rs` (`ViewIncarnation`, `DeadIncarnation`),
`crates/tessera-lifecycle/src/roster.rs` (minting), `crates/tessera-lifecycle/src/overlay.rs`
(the replay arm), `crates/tessera-store/src/manifest.rs` (`with_roster`, `is_live_incarnation`),
`crates/tessera-store/src/read.rs` (`with_views`), `crates/tessera-engine/src/write.rs`
(the fold's carry-forward). Tested at
`crates/tessera-server/tests/views_write.rs::a_recreated_key_holds_only_its_own_rows_across_a_replay_and_a_fold`
`::a_fold_after_a_drop_reclaims_the_dropped_view_and_a_recreate_adopts_nothing` and
`::a_drop_prunes_every_spelling_of_the_key_on_the_live_path_and_at_replay`, with
`tessera_store::view_path::tests::a_scoped_columns_path_moves_with_the_incarnation_and_not_at_the_build`
and `tessera_engine::coalesce::tests::a_dead_incarnations_window_is_not_planned` beside them.
