# 0130 — An artifact shard is a layer incarnation, with no reuse, and the identity carries a kind bit

**Date:** 2026-09-05 · **Status:** Settled (owner ruling)

## What this answers

As ruled in [sharding.md](../design/sharding.md) and
[decision 0126](0126-the-generation-counts-a-slots-occupancies-and-a-freed-slot-is-reused-before-a-shard-opens.md),
an artifact shard was one entity space with an allocator, a free pool and an occupancy count, on
the point-shard rule. An artifact's ordinal converts to its entity by arithmetic over a contiguous
run per level ([annotation-representation.md](../design/annotation-representation.md) §2.3), so
that pool would have been run-granular and best-fit; an individually deleted artifact would have
been a hole until its level dropped; the record blob would have had to reclaim a retired
artifact's content before a new occupant took the slot; and an edge storing a target's entity
could have resolved to a new occupant. The owner asked how an artifact shard, which has no rows,
should take advantage of looking like a row database, and where it has to differ.

## The decision

Three rulings, made together.

**An artifact shard is a layer incarnation.** Each incarnation holds a dense entity space of its
own, in which the layer's levels are contiguous runs under the existing arithmetic. In that space
it holds the overlay, own-term postings, attribute and record extents, the key locator, and the
level records, generating sets, contents and lineage the per-layer directory holds today. It
opens when the layer is registered, takes appends at each level's ordinal cursor while the layer
is current, and is dropped whole when the layer is replaced or withdrawn. It has no allocator
beyond the cursors, no pool, no generation, no retirement and no compaction. A deletion leaves its
overlay at the drop, which is the publication that removes its record; a suppression leaves it on
unsuppress or at the drop.

**The identity carries a kind bit.** `L₀` is `(kind: 1) ‖ (shard: 19) ‖ (generation: 12)` for a
point and `(kind: 1) ‖ (incarnation: 31)` for an artifact; `R₀` is the entity, 32 bits, in both.
Two counters in the manifest, `next_shard_id` and `next_incarnation`, are monotone and never
reused. The Feistel is unchanged.

**An attribute value an artifact carries is not an item on point surfaces.** Category counts,
browse and suggest sum over point shards. The value is served on the artifact's own row and
drill-down from its incarnation's extents.

⊘ Not built. Artifacts are allocated downward from `u32::MAX` in the one entity space, and the
per-layer directory holds records and memberships but no overlay, postings or extents of its own.

## Why a partition

The churn unit for artifacts is the layer. A replacement drops a layer whole and mints a
successor ([decision 0081](0081-a-replacement-mints-identities-an-edit-keeps-them.md)); an edit
keeps identity; an individual delete is rare. That is a partition workload, and the answer a row
store gives it is to drop the partition: Postgres's `DROP PARTITION`, Accumulo's delete-table.
Each incarnation is that partition. What the partition model removes from the artifact side is
everything decision 0126 needs for points: the pool and best-fit run allocation, the occupancy
count and retirement, content reclaim at reuse, the holes-until-level-drop rule, and compaction.
A dropped incarnation answers absent for every entity, so an edge into it cannot resolve to a new
occupant, and a replacement rotates only the dropped incarnation's fragment leaf.

Points keep the vacuum-and-reuse model of decision 0126 because their churn unit is the row: a
point shard whose items are deleted one at a time has no partition to drop, and a shard left with
its last live rows would otherwise hold its space for ever.

The artifact shard looks like a row database because Morton order is the one construction a row
store lacks, and artifacts have no rows to order. What remains is a partitioned heap with an
inverted index and a dead-tuple map, and it is stored as one. Where the design still differs from
a row store it differs for reasons that do not touch artifacts' storage: the visible set is a
materialised per-session bitmap rather than a per-row predicate, so every aggregate is computed
from inside it (I2) and execution time does not vary with what the principal cannot see;
membership is a bitmap over the point space rather than a join table, so an existence test is an
AND-cardinality; identity is the blinded physical slot rather than a logical key (I10);
publication is a generation swap rather than tuple versions; and suppression is a reversible deny
that survives compaction.

## Why a kind bit

One 20-bit counter shared by point shards and incarnations gives about a million layer
replacements over the life of a deployment: 287 years at ten layers a day, 12 years at ten an
hour. A replacement re-derives a clustering of the corpus, so the hourly figure is beyond any
pipeline that exists, but a cliff for one bit is not worth keeping. The bit comes out of the
point shard field, which falls from 2²⁰ to 2¹⁹ shards: 2⁴⁹ live rows at a 2³⁰ seal size, about
560T, and 2⁵¹ at the ceiling. The incarnation field gets 31 bits, about two billion replacements.
The two kinds have different lifecycles and different counters, and the encoding says so.

Modelled figures, from the counter widths and the stated rates:

| | Points | Artifacts |
|---|---|---|
| field | shard 19, generation 12 | incarnation 31 |
| what consumes it | net growth only (decision 0126) | one number per layer replacement |
| capacity | 2⁴⁹ live rows at a 2³⁰ seal | 2.1 × 10⁹ replacements |
| 10⁷ rows, daily ingest | one shard | a few replacements a year |
| 10¹⁰ rows, hourly ingest | about ten shards at 2³⁰ | replacement rate, not ingest rate |

## Why artifact values are not items

One entity space made an attribute one column and a category value one posting holding point
and artifact entities side by side, so an artifact with own terms carrying `topic = cardiology`
counted as one item in the `cardiology` total. Nothing intended that; it followed from the shared
space. Per-shard postings remove it: the count sums over point shards, and the artifact's value
sits in its incarnation's postings. Serving the value on the artifact's own row and drill-down is
unchanged, and decision 0104's filter bit is unchanged, since it is computed from members and
never from the artifact's own attributes.

## What this supersedes

- Sharding ruling E's second clause, that artifact shards open and seal on the point-shard rule,
  and `shard.seal_artifacts`.
- Decision 0126's clause that the artifact shard gains slot reuse: the occupancy count applies to
  point shards only.
- Ruling I's widths: the shard field is 19 bits behind a kind bit.
- The artifact half of `PUT` and `DELETE /control/shards/{id}`: an incarnation is dropped by the
  layer's own replacement or withdrawal.
- Conformance fixture (h) as first written.

## What this changes elsewhere

[sharding.md](../design/sharding.md) §1.1 (the artifact shard, its states, the counters), §1.2
(the kind bit and `Incarnation`), §1.3 (the incarnation directory and manifest fields), §2.2,
§2.5, §2.6 (the kind bit at inversion; ruling K on category surfaces), §3 (the layer publish),
§3.1, §3.2, §3.3, §3.4, §3.5, §4, §5, §6 (fixtures (h) and (j)), §7, §8, §9, §10 and §12.
[artifact-system.md](../design/artifact-system.md) §1's identity paragraph: an artifact's entity is
`(incarnation, entity)`. Annotation-representation §2.4: the per-layer directory becomes the
incarnation's, and holds its overlay, postings and extents; §4: artifacts keep entity ids; §5:
content reclaim is moot under replacement. Contracts §2.6 takes the encoding at stage S1, as
sharding §12 lists.
