# Artifact fetch: the questions, the fetch model, and what each side owes

**Status:** **Proposal r3 — the shape is the owner's, ruled and built 2026-08-28.** §5.2, §5.3
and §8's encodings are on the wire (contracts r43) and §6's fetch model is the shipped channel's
policy; the one ⊘ left is §7.3's server memo. Promotion is the owner's. A conflict with
[`contracts.md`](contracts.md) is resolved in its favour until then.

**Owns:** how a client asks for artifacts and what comes back — the scope of the question, the
default answer, the one opt-in projection, and the fetch model — together with what the client must
do to ask honestly, what the server must do to answer safely, and what a caller who has never read
this document is still guaranteed.

**Does not own:** what an artifact *is* ([`annotations.md`](annotations.md)), how it is stored and
evaluated ([`annotation-representation.md`](annotation-representation.md),
[`artifact-serving-at-scale.md`](artifact-serving-at-scale.md)), or the byte schema of the frames
([`contracts.md`](contracts.md) §3.2, which stays the contract — §5.2 and §5.3 argue changes it
took at r43; it states them).

**Reads with:** [`client-obligations.md`](client-obligations.md) rules 6 and 7,
[`delta-serving.md`](delta-serving.md) (as the shape the *future* replica-sync question would take,
not as a source of borrowed mechanism — see §5.4), [`artifact-cache-handover.md`](../artifact-cache-handover.md)
(the work's map and its traps), and decisions
[0030](../decisions/0030-determinism-is-not-a-guarantee.md),
[0076](../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md),
[0083](../decisions/0083-the-frontier-is-a-request-time-budget.md),
[0087](../decisions/0087-cross-level-edges-are-information-not-rollup.md),
[0103](../decisions/0103-a-request-naming-no-levels-is-answered-at-the-declared-ones.md) and
[0104](../decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md).

---

## 1. The one rule the rest is arranged around

**A caller who sends the plainest possible request gets a complete, self-describing answer, and
every mechanism below is something they may decline to use.**

That is not a courtesy to third parties; it is what keeps the surface auditable. The leak register
is exhaustive because the query surface is enumerable, and a surface where the meaning of a response
depends on state a client established earlier is one where no single request can be reasoned about
on its own. So each addition here is a *narrowing* the caller opts into: it can make the answer
smaller, never different in kind, and ignoring it costs bytes and nothing else.

Three consequences are load-bearing and are stated at their sites below: the server keeps **no
per-client state** (§7.3); a mistake costs a round trip, never a wrong map (§5.2); and that last
promise is an **admission test, not a description** — no opt-in enters this surface whose incorrect
use produces a plausible wrong map. Two shapes failed the test and were declined for it (§4); two
more sit behind it, deferred with the mitigation each would need priced at the site (§5.4). A
surface whose sharp edges fail loudly is one any client author can implement from the public
documents alone, which is a property this design maintains deliberately (§7.2).

## 2. The question space

Two axes, and they are already expressible.

**Scope** — *this view* or *the whole view*. The request's `bbox` (or `tiles`) says which, and the
whole extent is a `bbox` covering it. Nothing new is needed, and nothing distinguishes the two
server-side: a whole-extent request is an ordinary request whose box happens to be everything.

**Filter** — absent, or a `filters` expression. Absent means no question was asked about matching,
which is not the same as no artifact matching (decision 0104).

The cross product is four questions, and the corpus's own shape decides which is cheap:

| | no filter | with filter |
|---|---|---|
| **this view** | the ordinary map request | the ordinary map request under a search |
| **the whole view** | fetch a level whole, once | *which artifacts anywhere match* |

**The bottom-right cell is the one with no other bound.** A layer whose artifacts are scattered
through row space is returned in full at any viewport (`artifact-serving-at-scale.md` §7), so *this
view* and *the whole view* are the same answer there and the filter is the only thing narrowing it.

## 3. What exists today

Built, and the baseline everything below is measured against.

- **`layers`** names the layers to answer for; omitted or `[]` is none, `"all"` is every reachable
  layer, an array is intersected with the reachable set and never unioned (contracts §3.2).
- **`levels`** names the rungs; absent follows the layer's declared zoom→level map against the
  request's depth (decision 0103).
- **`artifact_budget`** cuts a nested layer's response structurally — ancestors in place of their
  descendants, never by sampling (decision 0083).
- **The *artifacts* frame** carries one row per served artifact: layer, id, key, masked count,
  geometry, content, parent, rung (§5.3 — `level` until r3), and `matched` — the filter bit, null
  where the request carried no filter (decision 0104).
- **`matched` is an Arrow boolean column**, bit-packed on the wire, positional to the rows in the
  frame it sits in. There is no addressing problem because the bits and the rows travel together.

**The client side** holds every payload it has been served under one identity key and content key,
keyed `(layer, tessera_id)`, and takes the bit from each response rather than from the store
(`clients/ts/core/src/artifactChannel.ts`; `artifact-cache-handover.md` step `cache 2`). Its whole
state is two structures and two keys — the served set, replaced wholesale per response, and the
payload store, accumulated beside it; the identity key and the content key, either of whose
rotation drops the store. It does not yet tell the server anything about what it holds, so the
server sends everything every time — and §4 is why that stays true.

## 4. Rows are the server's; columns are the client's

**The row set — which artifacts are in view — comes whole from a response, or is known whole
because the scope was fetched whole. It is never assembled from partial history. The client's store
may supply columns for rows a response named; it may never supply rows.**

The asymmetry is the payload's own: every payload column — key, masked count, centroid, box, hull,
content, parent, level — is a pure function of `(artifact, M_auth, generation)` and of nothing in
the request, and the content key hashes exactly those three things, so it rotates precisely when a
held payload goes stale and never because a client panned (`artifact-cache-handover.md` §2, where
each claim is traced to its site in the tree). Columns are therefore cacheable against the content
key. Which rows are in view is per-request and has no such key, and the served row set carries
contracts that quantify over *this response* — `parent_id` names a parent only where it is in the
same response, and the points frame's `membership:<layer>` column carries only identifiers present
in the same response's artifacts frame (contracts §3.2).

Two shapes violate the rule from opposite sides, and both were **declined by the owner
(2026-08-28)**. They are recorded here so they are not re-proposed.

**The server must not drop rows** (r1's "mode 1" — only the artifacts meeting the filter). A
*caller* dropping false-bit rows leaves the rest of the response intact; a *server* doing it does
not: a matched child's unmatched parent dangles, and `servedLineage`
(`clients/ts/core/src/artifactChannel.ts`) reads an unresolvable parent as a root, so the tree
reshapes with no error anywhere — matched sub-clusters draw as roots, the level picker collapses,
and the map looks plausible. The membership column is left naming absent ids. A keep-rule that
closes the references — matched, *or* named as parent by a kept row, *or* named by the membership
column — is exactly the contract pass the shape was claimed not to need. The caller's own version
of the projection has none of these problems, and remains available to every caller for free.

**The client must not reassemble rows from history** (r1's §5 — a held-set claim the server
subtracts). Three independent failures, any one of which is sufficient:

- *A cell's answer is not a function of the cell.* `prune_children` is the layer's declaration and
  applies unbudgeted, so an artifact served for a narrow box can be cut from a wider one; a client
  drawing *held-for-claimed-cells ∪ response* draws the parent's shape over its child with the
  parent's count beside it — the failure decision 0087 exists to prevent, and it looks like the
  cache working. The budget compounds it: the shipped client budgets every request
  (`artifactChannel.ts`), so it could never claim honestly at all.
- *Artifacts do not partition by cell.* Tiles partition row space, which is what makes the point
  path's declared-tiles form exact; an artifact belongs to many cells, so an honest per-cell claim
  needs per-payload provenance of the cells each arrived under, which no client records and the
  handover had already declined to make compact.
- *Even a perfect claim barely pays.* Measured through the frame as built, eliding a payload leaves
  97 of a row's 125 bytes on the wire — 22%, not the 60% r1's modelled figure supported (§8) —
  while the server pays a second candidacy walk over the claimed cells to compute the subtraction.

The one saving a claim offered that the rule permits — not re-sending payload *columns* the client
holds — is §5.2, with the caller asking rather than claiming.

## 5. The wire surface

### 5.1 The default answer

**A request naming a layer and a box gets every artifact that layer serves for that view, one
complete row each.** Built, the default, and it stays the default; this is the answer §6.1
guarantees to a caller who has read nothing.

### 5.2 The projection — built 2026-08-28

`artifact_rows: "full" | "identity"` on `/v1/viewport`, defaulting to `"full"` (contracts r43).

**`"identity"` answers with the same rows and fewer columns**: `layer` (dictionary-encoded),
`tessera_id`, `rung` (§5.3), `matched` — its own fixed four-column schema, measured at 13.6 B/row
against 125 for a full row (§8). The sentence that is the contract:

> **The row set is identical under either value of `artifact_rows`; only the columns change.**

That sentence is what keeps every cross-reference true — no parent can dangle and the membership
column still names ids present in the frame, because no row was dropped — and it is why this
projection discloses nothing: the response is a column subset of what the same caller's identical
request would have returned (§9). The absent columns are absent from the Arrow schema, not null, so
decision 0076's rule — a null means *this layer declares no such property*, never *withheld* —
gains no third reading.

**What it is for:** a filter change over a scope the client holds (§6). The held payloads answer
everything but the bit, and the bit is the one field a filter moves (0104).

**Misuse is self-detecting and lands on the client.** A caller who asks for `"identity"` while
holding nothing meets identifiers its store cannot resolve, knows it, and re-asks with `"full"` —
one round trip, no wrong map, which is §1's admission test passed rather than asserted. The server
neither knows nor cares what the caller holds; there is nothing to recompute, nothing to trust, and
no honesty rule to write.

**What the building settled** (Appendix R, r3): an identity response still runs the
content-servability probe, because an artifact whose content cannot be served is *absent* (0076)
— that is selection, not payload, and skipping it would have broken the contract sentence; what
it skips is materialisation only. The shipped channel takes this path for a filter over a scope it
holds whole, and re-asks in full, once, on the first identifier it cannot resolve.

### 5.3 The rung column — built 2026-08-28 (contracts r43)

The frame's `level` column became `rung`: **the declared level on a levelled layer, the
response-local parent-chain depth on a treed one, 0 on a flat one** — the number a client draws
by. Until then every client had to know that a levelled layer's resolution is its declared level
while a treed layer's is its chain depth, and pick per layer (`rungOf`; `artifact-cache-handover.md`
trap 5.4). The shipped client got that pick wrong once and was caught in review; every future
client would have rediscovered it. The server has both numbers at serve time. One column, computed
the right way per layer kind, and the trap stops existing for every client — the pattern §7.2
names: a trap documented for one client is a debt every client pays, a trap folded into the wire is
paid once. Levelled layers lose nothing (`rung` equals the declared level there); treed layers stop
encoding their resolution as something to be counted client-side, and `rungOf` is deleted. The
depth is computed **after every narrowing** — the budget cut, content withholds, the orphaned
dependent drop — over the forest the response's own `parent_id` links form, so a re-rooted
subtree's root reads 0; *response-local* is read strictly. Pre-release, so the column was renamed
and re-meant rather than appended beside its predecessor (0048).

### 5.4 ⊘ Deferred shapes — recorded with their triggers, deliberately unbuilt

**Bits alone, no rows** (r1's mode 3). The only shape whose answer cannot be read on its own: bits
with no rows must be positional to a set the caller reconstructed exactly, which forces a
contractual response order — ascending `(layer, tessera_id)`, a narrowing of decision 0030 for one
frame — plus a count and a digest of the ordered identifiers, with a client whose digest disagrees
discarding the bits and re-asking. Misalignment by one otherwise highlights the wrong clusters with
no error anywhere: the shape *fails* §1's admission test, and the digest is its mitigation rather
than a cure, which is why it sits last. Priced honestly: at the 464,655-artifact layer the bits
cost 116,552 B as Arrow writes them — values and validity buffers, 64-byte aligned — against
6.3 MB of identity rows, a ~54× saving. Its trigger is a scattered corpus where §5.2's identity
rows measurably hurt on filter interaction, which the model puts near 10⁷ artifacts (§8) — a scale
at which the client's memory and the map's legibility are failing alongside it (§6).

**Matched identifiers alone** — the ids whose bit is true, sized by the match count, so kilobytes
under a selective filter. Self-describing, no order contract, no digest. Its flaw is quieter:
absence means *unmatched*, which is sound only while the caller's hold really is whole, and a
caller wrong about that gets silently wrong bits — §1's test failed the same way, with no digest
even possible. Recorded behind identity rows, and behind the bits-only shape's honesty about
needing a mitigation.

**Delta sync** — *what changed since generation G*. If continuous ingest makes content-key rotation
frequent (§6's condition), the economic question stops being *what do I hold* and becomes *what
changed*, which is [`delta-serving.md`](delta-serving.md)'s framing and the one
`annotation-representation.md` §6 always assumed ("a level is replica sync rather than request
cost"). Nothing is designed; it is named here so the rotation-era work starts from the right
question rather than resurrecting a claim.

## 6. The fetch model — client policy, not wire

**Hold a scope whole where observation says it is cheap; pick in-view locally; ask per view
everywhere else.** A scope is a layer at a level over the whole extent, expressible since decision
0103. All of this is policy in the §7.2 sense — a client that ignores every word of it is correct,
and spends bytes.

- **Where a scope is held whole under an unrotated content key, pan and zoom cost no artifact
  traffic at all** — the row set is known without asking, because the hold is whole. Local picking
  draws the same set the server would have named, plus occasionally the edge of a shape large
  enough to cross the viewport while its visible members lie outside it — a boundary that ought to
  be drawn, since the geometry describes the whole visible membership and never claimed to describe
  the part in view (settled by the owner; `artifact-cache-handover.md` §4).
- **The first paint is per view; the hold follows in idle time, and the hold is ungated** (owner,
  2026-08-28). A settled view at scopes not yet held whole is answered per view — cheap at every
  zoom, the level map bounding the coarse rungs (254 artifacts at GeoNames' country rung, 4,842 at
  admin 1) and the tile index the fine ones (0.06 MiB at zoom 10, measured) — and then the channel
  fetches those scopes whole, one per idle window, whatever their size. There is no threshold: a
  count is not a bound in bytes, the two differing by orders of magnitude between a count-only
  layer and one carrying hulls — the reason the store itself carries no cap
  (`artifact-cache-handover.md` §6 step 2) — and the drop rules are the whole bound. What limits
  what a session holds is the declared map: a view names only the rungs declared for its zoom, so a
  session that never leaves the overview never holds admin 4. There is no cardinality hint and none
  is coming (S5, declined 2026-08-25, because a layer's artifact count is a corpus-wide count over
  objects the principal may not individually see), so every client decides from the same
  self-describing responses, which is what keeps §7.2's equality structural.
- **A scattered flat layer is held whole after its first response by construction** — every
  response *is* the whole layer, so the first paint costs the same bytes as today and the saving
  starts at the second settled view.
- **A deep-zoomed open fetches per view first and promotes to whole-rung in idle time**, so the
  hold is never paid at startup.
- **A filter change over a held scope** re-asks with the filter for identity rows (§5.2), taking
  the bit from the response and everything else from the hold.

**Where the walls are**, so the policy is sized honestly. Measured at GeoNames' scale; ⊘ everything
past it is modelled, the scattered corpus not existing (§8). On a scattered flat layer: per-view
re-send breaks first (~25 MB a settled view at 231,645 artifacts, modelled — the wall this policy
removes); filter-change identity rows carry to about 10⁷ (13.6 MB at 10⁶, where §5.4's bits would
take over); and at about 10⁷ the client's memory, the server's per-request candidacy and the map's
own legibility fail together **under every protocol buildable on this surface** — artifacts refuse
the sampling escape the point path uses, by design (0083: a structural cut, never sampling), so
past that wall the answer is the corpus declaring levels, not the wire. A tiered 10⁷ layer is
already served well: the declared map bounds every per-view answer and the policy holds whole
exactly the rungs where whole is small.

⊘ **The condition the economics rest on: rotation is rare.** The content key moves with the overlay
version, which moves on an ingest ([`delta-serving.md`](delta-serving.md) §2), and rule 7 drops the
store with it — so under continuous ingest this model degenerates to refetch-per-rotation. Points
get a softer treatment (a held band survives rotation, renderable and stale-marked); artifacts
today do not, and whether they should is an ingest-era question left open here, with §5.4's delta
sync named as the shape the answer would take.

## 7. What each side owes

### 7.1 The API user — what is guaranteed without reading any of this

**A request naming a layer and a box gets every artifact that layer serves for that view, each row
carrying its own identity, count, geometry, content, parent and level.** No ordering is assumed, no
prior exchange is referenced, and no state is established. Adding a filter adds one column. That is
the whole contract, and it is what `contracts.md` §3.2 and the OpenAPI description already state.

Every mechanism in §5 and §6 is a narrowing the caller opts into. Declining all of them is not a
degraded mode: it is the specified answer, and the only cost is bytes.

**What a caller must not assume**: the order of rows within the frame (decision 0030 — and the
bits-only shape is deferred precisely so this stays true), that two requests over the same box
return the same set when the content key has moved between them, or that an absent artifact carries
a reason. Absence is one answer with several causes, deliberately.

### 7.2 The client

**Three rules are the whole obligations set.** Each prevents a wrong map; nothing else on this
surface can cause one.

1. **Take the bit from the response, never from the store.** It is the one field a filter moves
   (decision 0104), and holding it answers this filter's question with the last one's.
2. **Drop held payloads when the content key rotates** — [`client-obligations.md`](client-obligations.md)
   rule 7, unchanged.
3. **Rows come from a response or from a scope held whole, never assembled from partial history**
   (§4). The store supplies columns for rows the response named; it never supplies rows.

**Everything else is policy** — when to hold a scope whole, when to project, prefetching, the
client-side spatial index a large hold wants — and getting policy wrong costs bytes, never
correctness. The split is deliberate and is this design's answer to a question the owner posed:
the provided client must not become *required*. A zero-policy client is correct in a page of code
and, with §7.3's defaults, acceptable against every corpus that exists; the TypeScript client is a
policy bundle over the same public surface, holding no privileged signal (S5's decline means there
is none to hold); and the Python SDK stays at zero policy as the standing proof — the day it needs
the policy stack to be usable is the alarm.

### 7.3 The server

- **Keep nothing between requests.** No client registry, no served-set memory, no session-scoped
  cache of what was sent. Everything a response depends on is in the request, the mask and the
  generation — which is what makes a response reasonable about on its own, and what keeps the leak
  register enumerable.
- **Nothing a caller sends may widen an answer.** A projection removes columns; `levels` and
  `artifact_budget` bound rows each artifact already cleared its own criterion for (0083, 0103).
  There is no request field whose misuse serves more.
- **Defaults carry the caller that holds nothing.** The behaviour every client would need is a
  server default, never a client obligation: the declared zoom→level map answers the absent-`levels`
  case (0103), and §8's encodings cheapen every response identically for `curl` and the shipped
  client. ⊘ **A derived-geometry memo** — keyed `(artifact, M_auth, generation)`, the same three
  things the content key hashes — would make repeat per-view requests cheap for callers that cache
  nothing, with no wire field, no client obligation and no register pass; it is internal, unbuilt,
  and unmeasured (`artifact-cache-handover.md` §3's "nothing caches derived geometry" is the gap it
  would close). It is named here because it is the naive client's insurance, not because this
  design depends on it.

## 8. Cost — measured, and what is not

**Measured 2026-08-28**, through `tessera_wire::payload::artifacts_frame` at 464,655 rows with a
15-byte layer name and the real Arrow `StreamWriter`, in a throwaway crate outside the tree — not
committed, and not a probe; to re-run, build a crate with a path dependency on
`crates/tessera-wire`, construct the rows, and take the frame's length. The identity-row figure was
re-derived independently from the Arrow buffer layout and came to 96.5 B/row against the measured
97.1 — the two agree, and both disagreed with r1's model, which is what reworked this document.
(The figures were taken for r1's review; the review memo was deleted once dispositioned — Appendix
R — so their provenance lives here.)

| shape | B/row | at 464,655 |
|---|---:|---:|
| full row (key, one content string, centroid, box, no hull) | 125.0 | 58.1 MB |
| identity and bit **through the frame as built**, every nullable column null | 97.1 | 45.1 MB |
| a reduced four-column schema (layer utf8, id, level, matched) | 31.6 | 14.7 MB |
| the same, `layer` dictionary-encoded — **§5.2's schema** | **13.6** | **6.3 MB** |

The 97.1 is why §4 declines payload elision within the fixed schema: the frame writes one
16-column schema uncompressed (`payload.rs`; the server compiles `tower-http` with `cors` alone,
so there is no transport compression either), a nulled fixed-width column still writes its slot,
every nulled variable-length column still writes a four-byte offset per row, and `masked_count` is
non-nullable. Eliding a payload through it saves 22%. The 13.6 needs a second schema, which is why
§5.2 proposes one.

**Two encoding changes, built 2026-08-28, no protocol**: dictionary-encoding `layer` (u16 keys;
~14% of **every** response, 125.0 → ~107 B/row) and moving the two hull columns to the tail of the
schema, **absent from it** when no served layer declares a hull (~7%); together 125.0 → 98.8 B/row,
measured. They help every caller identically and preceded everything else here in value per unit
of anything. The gate now holds the figures: at 100,000 synthetic rows with a 15-byte layer name,
identity **14.6 B/row** (bound: under 20), the hull-free full row **97.9 B/row** with the
dictionary, and the layer column **2.1 against 19.1 B/row** plain — measured on both encodings,
not modelled (`crates/tessera-wire/tests/wire.rs`).

**Whole-extent measured figures** (`../evidence/memos/2026-08-28-artifact-response-volume.md` §1):
49.0 MiB and 1,887 ms for 464,655 artifacts under a full principal at zoom 0; 0.06 MiB at zoom 10;
0.03 MiB at zoom 0 under the declared map's default (decision 0103). The time is derived-geometry
CPU over the whole visible membership — §7.3's memo is the internal lever on it, unmeasured.

⊘ **Unmeasured**: the scattered flat layer that gives §2's bottom-right cell its teeth — no corpus
declares one at scale; an attribute layer over GeoNames' 231,645 `admin4` values would produce
one, and that is a corpus change rather than a code change (`artifact-cache-handover.md` §6 step
4). Every figure above 10⁶ artifacts in §6's walls is modelled from the per-row numbers here, and
none should be quoted as a performance result.

## 9. Disclosure

**No leak-register row is proposed, and here is the reasoning for each piece.** Appendix C's
inclusion test is that a row exists only where a viewer, reading responses they are entitled to,
can end up knowing something about data they were **not** served; data the service serves never
qualifies.

- **The projection** (§5.2) returns a column subset of what the same caller's identical request
  would have returned. Strictly less, computable by that caller from the full answer's own columns.
- **The rung column** (§5.3) re-derives a number every client already computes from `parent_id`
  links the response carries. Nothing new crosses.
- **Existence and the masked count remain anchored on `M_auth`** through all of it. A filter
  narrows what is *sent*, never what exists or what is counted (**I3**, **I12**), and the level and
  the budget are request bounds over artifacts that each passed their own criterion (decisions
  0080, 0083, 0103).
- **No response reports a withheld or already-held count**, under any shape here or in §5.4 — a
  caller could probe with one. The projection makes the rule easy to keep: no rows are withheld, so
  there is no count to be tempted by.
- ⊘ **The bits-only shape's contractual ordering would need its own look** if it is ever built:
  ascending `tessera_id` is chosen precisely because it is order-free with respect to ordinals,
  which C8 keeps off the wire — but it is a new ordering guarantee and should be argued at the
  time rather than inherited from here.

## 10. What is decided, and what is not

**Decided and built** (2026-08-28): the filter bit and its shape (0104); the level selector and the
declared map as its default (0103); the client's payload store and its drop rules (`cache 2`).

**Decided by the owner, 2026-08-28, in the discussion that reworked this document, and built
the same day:**

- **A held-set claim and server-side row dropping are declined** (§4). Neither returns in another
  costume; the row/column rule is the test a successor must pass.
- **The projection is the one wire affordance** (§5.2), and the rung column moves the one
  client-side trap into the wire (§5.3). Both built; contracts r43.
- **The fetch model is policy, not obligation** (§6); the obligations are §7.2's three rules. The
  shipped channel implements the policy, and **its idle promotion carries no gate** — a threshold
  counted in artifacts was proposed at 10,000 and refused: not a bound in bytes, and the drop
  rules are the bound, as for the store.
- **§8's two encodings** — built; they preceded the rest, and the gate holds their figures.

**Open, and each is the owner's:**

- **The ingest-era pair** (§6's rotation condition): whether artifacts get the point path's softer
  stale-marked treatment, and delta sync (§5.4) — both premature until continuous ingest is close.
- **§7.3's memo** — the one ⊘ left, internal, waiting on a measurement that says the whole-extent
  time matters to a caller that holds nothing.
- **Promotion of this document**, which is a normal review-and-promote and not a re-litigation of
  the shape.

## Appendix R

**r3 — 2026-08-28.** Built, the same day as r2, on the owner's direction, in four seams: the wire
and server (§5.2, §5.3, §8 — contracts r43, the OpenAPI description, the oracle's reader); the
channel's fetch model (§6); the TypeScript client's decode and its identity path; the Python
readers and the conformance run. What the building changed, each recorded at its site: an identity
response still runs the content-servability probe, that being selection rather than payload
(§5.2); the rung is computed after every narrowing, not the budget cut alone (§5.3); hull presence
is decided from the served rows, which is equivalent to *a served layer declares a hull* because a
served artifact always has a visible member and a declared hull then always computes; dictionary
keys are `u16`; `api_version` stays at 1 on deviation 10's argument as the r26 framing rework's
precedent, the schema itself being the loud break. Two things the client work found: the shipped
channel had **never sent `filters`** on the artifact request, so decision 0104's bit had never
reached the client it was built for — closed as a consequence of *a filter always asks the
server*; and a channel answering views locally issues no request of its own, so the point path's
observed content key is now handed to it, which is the only route by which rule 7 can fire while a
hold stands. The promotion ratchet was built with a 10,000-artifact gate and **the owner refused
it** — not a bound in bytes, the drop rules being the bound as they are for the store — so it was
removed and §6 rewritten (§10). The measured figures in §8 are the gate's, and stand beside the
review's.

**r2 — 2026-08-28.** Reworked the same day as r1, after its review and an owner discussion that
settled the shape. r1 posed three filter modes and a held-set cache claim; r2 replaces both with
§4's row/column rule, §5.2's projection and §6's fetch model, on the owner's acceptance of the
argument that every defect the review found was a case of a client supplying rows. The review — one
independent reviewer on performance, the API user's and client author's experience, and simplicity;
fourteen findings — was dispositioned in one pass and **the review memo deleted at the owner's
direction**, its measured figures carried into §8 with their provenance. Dispositions: F1 (byte
model wrong 2.4×) accepted — corrected in §8, and it is the number that declined the claim; F2 and
F3 (elided rows collide with 0076's null rule; mode 1 breaks `parent_id` and the membership column)
accepted by declining every row-supplying shape, §4 — the identity schema survives as §5.2's
caller-asked projection, where neither defect can occur; F4, F5 and F10 (the budgeted client could
never claim; `prune_children` breaks the claim's union; artifacts do not partition by cell)
accepted, recorded as §4's second decline; F6 (the no-protocol server memo never priced) accepted —
§7.3, named as the naive caller's insurance; F7 (server cost misattributed) moot with the claim,
the grid's exclusions now cited nowhere; F8 (the borrowed idiom unbuilt, mis-cited, inverted)
accepted — delta-serving is now cited only as the future replica-sync framing, and its
no-elided-count rule is kept at §9; F9 (rotation may make the cache inert) accepted — §6's ⊘
condition; F11 (mode-3 bits 2× low) corrected at §5.4; F12 (MiB/MB) corrected at §8; F13 (the
status record repeats the wrong figure) applied to [`client-delivery.md`](../client-delivery.md)
and the map's §4a in the change that landed r2; F14 (a recommendation beats a fourth open question)
overtaken — §10 records rulings.

**r1 — 2026-08-28.** Written after the owner asked for the whole artifact-fetch space in one place,
following the two steps that landed that day (the filter bit; the client's payload store). Its
shape was the four questions, three filter modes, and a cache claim borrowed from the point path;
its byte model was modelled rather than measured, and the review measured it wrong by 2.4× in the
direction that reversed §5's argument. Three things it settled at their sites survive r2: a
bits-only mode forces a contractual order and a digest and is deferred; the claim and
`artifact_budget` do not compose; a filtered elision may never touch identity.
