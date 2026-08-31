# 0116 — A scoped value's address is (attribute → its group, key), and the join rule is decided on the serial writer

**Date:** 2026-09-01 · **Status:** Settled (owner rulings, 2026-09-01) · **Amends:**
[views.md](../design/views.md) §4, §5 (r27); [contracts.md](../design/contracts.md) §3.1 (r68)

Two rulings, taken together because they land in one place: the point at which `/control/ingest`
decides what a row may carry.

## 1. The address is the key, not the view

### Context

`views.md` §5's write half admitted a group-scoped attribute family's columns only on a batch whose
view the **owning** group holds. A group declaring `members` of the owner shares the owner's keys
under its own view ids; its rows *render* the family and its leaves *filter* on it, but its batches
could not write it — the column was reached, on that rule, only through the owner's door.

The argument for the one door was a jam: the column a sharing view's value would land in is the
owner's, addressed by the shared key, so a batch through the sharing view and one through the
owner's view of the same key would each write a value for one entity into one column — two layers
claiming one entity, which the layer composition refuses.

### The decision

**The key addresses the cell, and the view does not.** A scoped value belongs to
`(entity, attribute → its group, key)`. A batch whose view's key belongs to the attribute's group's
key set — through the owner **or** through any group sharing those views — may carry the family's
columns, and the value lands in the one cell. A view whose key is in no such set refuses the column
exactly as before, whatever it is spelt: that refusal is what keeps a scoped column un-nameable
outside the views its key addresses, and it did not change.

**The jam is dissolved, not overridden.** What keeps one claimant per cell is a comparison on the
serial writer, not a closed door:

- the cell is empty, or the row names no value for it → the row writes it;
- the cell holds the **same** value → the row's copy is dropped. One claimant, so the extents stay
  disjoint in entity space and the composition has nothing to refuse;
- the cell holds a **different** value → 409 naming the column and the key, before the WAL append,
  whole batch without effect.

The refusal names no group. A caller writing through a sharing group's view learns that the key
already holds a value — its own request measured against the published schema — and nothing about
who owns the family.

**The same-window case needs no separate check.** Two batches writing one cell means two rows for
one entity, so the second names an external id the open commit window already holds; the executor
closes and applies the window before admitting it, after which the first batch's row is in the
buffer and the comparison's buffered source answers. The cross-window case is the flushed source of
the same comparison.

### Consequences

- Admission maps a batch's view to its key and the owning group through `EngineMeta::owning_key` —
  the same resolution the filter surface and the render list ask, rather than a second one.
- The WAL and buffer keying is unchanged: a row's `scoped` list stays positional against the owning
  group's `scoped_scalars`, so a sharing door's record is byte-identical to the owner door's but
  for the view field.
- The flush addresses a scoped column by the **owner's** view id (`scoped_owner_view_of`), so a
  sharing door's extent is written to `attrs/<column>/<owner group>/<key>/` and read by the leaf
  that resolves there. Its own rows carry the value in their lane, where they used to carry an
  absence.
- ⊘ What no flush can do is fill in a row tail a previous flush already published. An entity with
  rows in two views of one key, written through one door and joined through the other *after* that
  door's flush, renders the value under the writing view and the placeholder under the other. The
  filter answer is the cell's under both, the operand being the entity-space column. A build writes
  both lanes from the one column and has no such asymmetry.

## 2. The join rule is decided on the serial writer

### Context

`views.md` §4's label and attribute arms ran in `/control/ingest`'s request handler, before
submission. The map that decides which rows *are* joins — the live external-id map — is written at
**apply**, a whole executor drain later, and the write executor already carried an apply-adjacent
backstop (`LiveState::established_collisions`) that re-decided join-ness there. So a row the handler
called new and the writer called a join skipped both arms: it arrived with its descriptors intact
and re-labelled the entity with no overlay entry. The render backfill had already been placed on
the writer for exactly this reason, and its placement argument is the same one.

### The decision

**One authoritative site, on the serial writer.** The label arm, the entity-scoped attribute arm
and the new scoped cell arm all run in `WriteExecutor::admit`, immediately after
`established_collisions` has settled which rows join, beside the generation the apply will clone
from. The drop of a joining row's descriptors and terms moves with them, for the same reason and in
both directions: a row promoted to a join must still arrive with descriptors for that site to drop,
and a row demoted from one — its holder deleted in between — must still arrive with a label.

**The handler's arms are deleted, not duplicated.** What stays there is what does not depend on
holder state, plus two things that do and must:

- the **duplicate** answer — a known id already holding a row in the named view — because it is the
  one refusal that may *name* the caller's own ids, which an executor refusal may not
  (**I10**, `error.rs`'s standing rule). The executor's backstop keeps its own count-only form.
- the **sidecar half of the join resolution**, which only the handler can answer and which cannot go
  stale: the bundle's external-id extents are immutable, and the executor's backstop deliberately
  re-reads the live map alone.

**The ack path is unchanged and the 409 is synchronous.** `/control/ingest`'s contract is
parse → allocate → WAL append → fsync → apply+swap → 200, and the handler blocks on the executor's
receipt. A refusal is taken before the append, so the caller receives the 409 in the same request
and the batch leaves no WAL record, spends no entity id and moves nothing.

**The refusal bodies did not move with the site.** They are the handler's own text, byte for byte,
mapped through a new `ExecError::JoinRefused` whose detail is passed to the caller unchanged — the
same standing `LayerRefused` and the view verbs' refusals have, and for the same reason: a row
index, a column name and a view key are the caller's own request measured against the published
schema. There are byte-identity tests over these bodies and they are unchanged.

### Cost

The oracle reads the arms make — the entity→term transpose, and `flushed_scalar`'s three homes —
now happen on the writer thread rather than on a `spawn_blocking` handler. The write-latency budget
is seconds and more for both ingest and denies, so this is inside it; the reads are one per joining
row and most batches carry no join at all, which is the branch that skips the whole pass.

## What this does not change

- Which columns a batch may name outside a scope. An unrelated view's refusal is the
  undeclared-column 422, byte-identical to before.
- The gate. Which views a principal may reach is `views.md` §6's and is untouched by either ruling.
- Any artefact format. A sharing door writes the extent the owner's door already wrote, in the
  directory it already wrote it in.
