# Artifacts declared by the points that belong to them — design

**Date:** 2026-08-19
**Status:** Draft — owner-ruled in discussion, **§2, §3 and §4 built at build time**. The readers
take an integer key, skip a noise one, and read a list column as the artifacts a point belongs to
plus the edges between them; `value_set` decides whether a member key may create an artifact, at a
build. ⊘ **§6 (the ingest half of `value_set`) and §5's write-path rulings are unbuilt**; a layer
declared `open` therefore governs what a *build* mints and nothing at ingest, where an unknown key
still has no route in. The rulings in §5 are the owner's; the rest follows from them. Extends
[`configuration.md`](configuration.md) (normative for the surface) and
[`annotation-write-cycle.md`](annotation-write-cycle.md) §6.1 (normative for artifact semantics).

## 1. What this answers

A clusterer emits a label per point: one cluster id in a column, or a list of them for a
hierarchical run. The corpus that produced it is a point table, and the clusters exist only in the
sense that points reference them. Today that has to be turned into an artifact table plus a member
table before Tessera will read it, and any cluster the producer forgot to enumerate is a refusal
rather than a cluster.

The whole of this design is: **a cluster exists because points say it does**, and everything else
about it — its name, its parent, its content — is optional enrichment from wherever the caller
already keeps it.

Four cases, and the third is the one nothing expresses today:

| Roster | Enrichment | |
|---|---|---|
| a table | that table | the shape that exists |
| the points | none | a bare clustering |
| **the points** | **a table** | **a clusterer plus whatever is known about its clusters** |
| a table | elsewhere | not a case anyone has |

## 2. Membership needs no new surface

`[layer.members]` already means *one row per `(artifact, entity)`* and already reads `key`,
`entity`, and optionally `rank` and `level`, with `fields` naming which columns carry them. A point
table with a cluster column **is** that shape:

```toml
  [layer.members]
  source = "points"
  fields = { key = "cluster_id", entity = "id" }
```

The source is named, not inferred — a membership route that silently read some other block's file
would be the one thing about this that a reader could not see. A hierarchical run is the same
block over an exploded table carrying `level`.

Two readers change, and neither is about membership:

- **A key column may be an integer.** Cluster ids are integers, and requiring UTF-8 makes the
  common case both awkward and slow: converting per point costs ~54 s at 10⁹, against ~15 s for an
  integer hash and ~0.5 s where the value is already an interned code (measured, 10⁵ distinct ids,
  single-threaded). The roster is converted once; the per-point path never formats anything.
- **A null key, and `-1`, mean *this point is in no artifact*** — skip the row. Noise is a quarter
  of the points at each split of a condensed tree on the corpus measured here, and `-1` is the
  sentinel every clusterer emits. Exactly `-1`, not any negative: a negative id is otherwise
  unusual enough that swallowing `-7` would more likely be eating data than handling noise. For a
  string column, null only.

## 3. `value_set` decides whether a key may create an artifact

```toml
[[layer]]
artifacts  = "clusters"        # enrichment: names, parents, contents — optional
value_set  = "open"            # a key no artifact declares creates one
membership = "enumerated"
```

`value_set` is the word [`configuration.md`](configuration.md) already uses for exactly this
question on a vocabulary — *is an unknown key at ingest refused, or minted?* — applied to a layer's
artifacts. `closed` is the default and is today's behaviour: the roster is `artifacts`, and a key
not on it is refused because a mistyped id would otherwise publish a phantom artifact carrying the
members it stole from a real one, whose masked count then goes quietly short.

**It sits on `[[layer]]`, not on `[layer.members]`, because ingest has no member block.** A key in
a build-only acquisition block could not govern what the write path does with an unknown id, and
governing both is the point.

With `open`, `artifacts` becomes enrichment rather than a roster. A cluster in the table that no
point references is an artifact with no members — subject to the layer's existence criterion, so
generally not served, and generally a sign the table is stale. A cluster the points reference that
the table omits exists with no title and whatever `content = { computed = … }` gives it. Neither is
an error.

## 4. A lineage list declares the edges

Where the column is a list, its shape is checked against the hierarchy kind the layer already
declares — the kind is declared as it is today, and the edges are read from the data the caller
supplied. **Every entry names an artifact the point is a member of**, exactly as a scalar key names
the one; what the kind supplies is what the *positions* mean:

| `hierarchy.kind` | Column | Edges |
|---|---|---|
| `flat` | scalar, or a list read as plain multi-membership | none |
| `stacked` | fixed-length list, one entry per level | none — independent analyses |
| `tiered` | fixed-length list, one entry per level | containment, between consecutive entries |
| `nested` | variable-length list | the lineage: entry *k* is the parent of entry *k+1* |

Fixed-length entries are nullable: a point may be noise at a fine resolution and clustered at a
coarse one. A variable-length list against `tiered`, or a fixed-length one against `nested`, is a
refusal rather than a guess.

**A child naming two different parents is refused.** The map is built during the pass that already
walks the column, and a conflict means the data is not the tree the layer declared — there is no
correct output, and choosing a parent would publish a hierarchy the caller did not write. A `parent`
column on an artifact row and a lineage column disagreeing about one artifact is the same conflict
and refuses with the same words: they are two spellings of one edge.

Three things the table does not settle, decided in the building:

- **A fixed length is the declared level count, however the file spells it.** An Arrow
  `FixedSizeList` states its arity in its type and is checked once against `[[layer.levels]]`; a
  plain list states it a row at a time, and a row of another length is the refusal. Reading the type
  alone would refuse every producer whose Arrow binding writes a plain list, which is most of them —
  the declaration and the data agree or they do not, and the type is only one of the two places the
  data can say so.
- **An entry naming nothing links nothing across itself.** The edge is between *consecutive*
  entries, and an entry that is null or `-1` is not one — so a point clustered at level 0 and level
  2 and noise at level 1 declares no edge at all, and an artifact only ever named that way is
  published as a root. Reading past the gap would state a containment no row makes, which the next
  point clustered at that resolution would contradict.
- **A row whose every entry is noise is one row in no artifact**, counted in §2's number rather than
  a second one: rows are the denominator that separates a noisy clustering from a wrong column.

A `level` column beside a list key column is **ignored, and said so**. The positions are what carry
the levels, so reading it too would place a point at a level its list did not name; refusing would
block a build over an input that discloses nothing and costs a rerun.

## 5. Identity, and what a suppression survives

Four rulings, owner-made:

**A key is a name; identity is what minting allocates.** Delete an artifact and reload a point
carrying its key and you get a **new artifact** — decision
[0047](../decisions/0047-edit-is-delete-plus-reingest.md) and
[0081](../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md) applied, not a new
rule. Uniqueness is *at most one live artifact per key per level*; tombstoned ones do not count. The
build's existing refusal of one key on two rows is within a build and stands.

**A suppressed artifact still exists, and ingesting a point carrying its key changes nothing.** The
point joins the membership; the artifact stays suppressed.

**Minting therefore tests against live artifacts, not servable ones.** Written the natural way —
*is this key unknown?* — against what is currently served, a suppressed artifact reads as absent, a
second artifact is minted under its key, and the new one is not suppressed. A suppression would be
defeated by ingesting a point. This is the only fail-open in the design and it is closed by
construction rather than by a check.

**Deleting an artifact deletes its suppressions**, and the removal rides the deletion's own entry —
hidden at the same ack, retired at the same compaction fold. Not a separate sweep: a sweep is a
second retirement route for suppressions, and if it ever ran ahead of the deletion becoming visible
the artifact would serve unsuppressed in the gap. One removal rule, as write-path §5.4 requires.

## 6. Build and ingest are one rule at two entry points

⊘ **The ingest half is unbuilt.** `value_set` is carried on the layer declaration — which is what
the write path reads, and why it is not on the acquisition block — and is consulted at a build only.

Everything above holds identically whichever way a point arrives. At ingest a point may carry a
cluster id, or a lineage naming clusters that do not exist yet; under `open` the chain is minted and
linked in one batch, **parent before child** — the ordering constraint edges already carry
(`annotation-representation.md` §5.0.4), applied to a batch rather than to a build.

Computed content needs nothing extra: centroid, box and hull are recomputed per viewer from current
membership, so they follow new points without invalidation.

## 7. Joins are reported, not refused

Any file carrying entity ids joins to entity space by id — geometry, members, attributes alike — and
a row naming an entity this build did not load is **ignored**. That is the ordinary case: an
attribute file covering two million documents against a fifty-thousand-document build is a superset,
not an error.

Ignoring silently is the failure this system has already shipped once, in the extent that folded a
corpus into a corner while the build said nothing. So the numbers print, against the denominator
that means something — **entities covered**, not rows dropped, because a legitimate superset and a
broken join drop the same overwhelming fraction:

```
attribute 'sentiment': 49,812 of 50,000 entities have a value
        1,950,188 source rows named entities this build did not load
```

Zero of fifty thousand is a wrong column or a wrong file, and warns loudly. It does not refuse: the
operator is present, the loop is fast, and a bundle with one unpopulated attribute is a rerun rather
than a mission.

## 8. Open

- Whether a minted artifact that loses its last member should withdraw by default. The object *was*
  its membership, unlike a curated set, so the layer default may want to differ — undecided.
- How the roster appears in `reports/disclosure.json`. A diff reading *417 clusters, unchanged*
  while every identity churned would mislead in exactly the case that matters.
- The notification owed when a pipeline rerun deletes a cluster someone had suppressed. The
  suppression is gone by design (§5) and the operator who applied it is not the one who ran the
  pipeline. This rides write-path §5.8's existing obligation for deletions that dark-ship, rather
  than needing machinery of its own; ⊘ that report is unbuilt.
- A per-layer bound on minted artifacts, declared rather than enforced, so a corrupt column warns
  loudly instead of minting millions. Undecided whether it is worth the key.

## 9. Not in scope

**`membership = { attribute = … }` stays declared and unbuilt**, and is a different feature: it
makes membership a *predicate evaluated per request*, where this design materialises at build and
maintains at ingest. The two look alike from the declaration and differ in where the work happens —
a predicate over a `derived` vocabulary is answered by a masked scan, which a viewport showing 263
clusters would pay 263 times.

## Appendix R — review trail

**2026-08-19 — r3. §4 is built, and the byte-identical assertion is what says it reads structure
rather than inventing it.** A `nested` lineage column and the same clustering written out as an
artifact table with a `parent` column and a member table build the same bundle down to the byte,
and so do a `tiered` fixed-length column and a member table carrying a `level`. Every entry is a
membership and the kind supplies the positions' meaning; minting composes with §3, so an interior
parent that only ever appears inside somebody's lineage is minted like any other key. Three points
the section did not settle were decided in the building and are now written into it: what counts as
a fixed length (the declared level count, from the Arrow type where the file gives one and from the
row otherwise), that a null entry breaks the adjacency rather than being read past, and that a row
of nothing but noise is one unclustered row. One refusal was added beyond the ones §4 states — a
lineage column contradicting a `parent` column, which is §4's own conflict arriving by two routes —
and one thing that could have been a second is a warning instead: a `level` column beside a list is
ignored and printed, on the rule that only a disclosure or an irreversibility earns a refusal.

**A list under `flat` was briefly refused and is not.** The reasoning was that a flat layer has no
positions for a list to index, which is true and does not follow: the entries are still
memberships, and a point in several artifacts of one flat layer is what several rows of a member
table have always meant. Refusing the list spelling would have made two spellings of one
membership disagree — the property every other input route here is held to — and would have
foreclosed overlapping groupings, a document under three topics being ordinary rather than a
mistake. `a_list_on_a_flat_layer_is_multi_membership_and_matches_a_member_table` pins it byte for
byte.

**2026-08-19 — r2. §2 and §3 are built at build time, and §2's first claim was already true.** The
membership route needed no code: a `[layer.members]` block pointed at the points file, with
`fields = { key = "cluster_id", entity = "id" }`, already built the same bundle as the same layer
declared with a member table — asserted byte for byte, which is how the other input spellings are
held. What the stage added is the two readers (an integer key canonicalised to its decimal string,
converted once per artifact and never per point; a null key and exactly `-1` skipped and counted)
and `value_set` on `[[layer]]`, carried on the layer declaration so the write path has the same key
to read when §6 is built. One refusal was lifted rather than added: a member source with no
artifacts source is legal under `open`, which is the bare clustering of §1's table. The count of
skipped rows is printed and reaches the build report; the join-coverage report of §7 remains
unbuilt.

**2026-08-19 — r1.** Written from a discussion that reversed several of its own conclusions, and
the reversals are worth recording because each was a wrong instinct with a recognisable shape.
*Attribute membership* was reached for first, because `membership = { attribute = … }` fits the
data; it was the wrong mechanism, being a live predicate whose per-request cost nobody asked for
(§9). *A vocabulary as the roster* followed, and fell to the observation that if the column is not
an attribute it does not want a vocabulary either — which removed the multi-valued-attribute
dependency that had made §4 look blocked. *A `column` key* reading implicitly from the corpus was
proposed and rejected: the source is named. And the roster was twice conflated with enrichment,
first in one `artifacts` key and then in a reserved word, before §3 separated them.

The residue is small: one capability (`value_set` on a layer), one reading of a list column, two
reader fixes, and four rulings. Everything else the requirement needed was already in
`[layer.members]`.
