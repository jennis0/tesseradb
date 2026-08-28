# Review — `artifact-fetch-protocol.md`, the fetch surface proposal

**Status:** Evidence, never normative. **This records a review and disposes of nothing.** The
document it reviews — [`../../design/artifact-fetch-protocol.md`](../../design/artifact-fetch-protocol.md),
r1, 2026-08-28 — is unchanged; every finding below stands open against it.

**How it was done.** One independent reviewer, briefed on three axes in this order — performance, the
experience of an API user and of a client author, and simplicity against the house standard that an
optimisation costing reviewability needs an argument. The reviewer read the proposal, its map
([`../../artifact-cache-handover.md`](../../artifact-cache-handover.md)), the decisions it leans on,
and the built code, and measured the frame rather than trusting the proposal's model.

**Provenance of the numbers.** Rows marked *measured* come from the reviewer calling
`tessera_wire::payload::artifacts_frame` at 464,655 rows with a 15-byte layer name through the real
Arrow `StreamWriter`, in a throwaway crate outside the tree — **not committed, and not a probe**. To
re-run: build a crate with a path dependency on `crates/tessera-wire`, construct the rows, and take
the frame's length. The identity-row figure was **re-derived independently from the Arrow buffer
layout while dispositioning this review** and came to 96.5 B/row against the measured 97.1, which is
the check that matters: the two agree, and both disagree with the proposal.

---

## The headline

**The proposal's byte model is wrong by about 2.4×, in the direction that reverses the argument it
supports.** §5 and §8 cost an identity-and-bit row at "about 40 bytes as the frame stands today" and
about 12 dictionary-encoded. Measured through the frame as built, it is **97 B**, against 125 B for
a full row: payload elision saves **22%, not 60%**.

The reason is elementary and worth stating so it is not repeated. The artifacts frame writes **one
fixed 16-column schema**, uncompressed (`payload.rs::artifacts_frame`; the server compiles
`tower-http` with `cors` alone, so there is no transport compression either). A nulled fixed-width
column still writes its full slot, every nulled variable-length column still writes a 4-byte offset
per row, and `masked_count` is **non-nullable** and so cannot be nulled at all. The proposal counted
the columns identity needs and treated the other eleven as free.

Measured, 464,655 rows:

| shape | B/row | total |
|---|---:|---:|
| full row (key, one content string, centroid, bbox, no hull) | 125.0 | 58.1 MB |
| identity + bit, every nullable column null, **through the frame as it stands** | **97.1** | **45.1 MB** |
| a reduced 4-column schema (layer utf8, id, level, matched) | 31.6 | 14.7 MB |
| the same, `layer` dictionary-encoded | 13.6 | 6.3 MB |

The proposal's 12 B and 40 B are reachable only under a **second schema for the artifacts frame**,
which its own *Does not own* line disclaims ("the byte schema of the frames … stays the contract")
and which §5 never proposes.

---

## Findings

Each is typed *defect* (the design is wrong or will fail), *risk* (it may fail under conditions the
document does not name) or *preference*.

### F1 — performance · defect · §5, §8, and the map's §4a.1

The byte model above. **It bites hardest on the one case the proposal asks the server to do extra
work for**: a filtered settled view over the 231,645-artifact scattered layer goes from ~25.5 MB to
~22.5 MB with a perfect claim, while the server pays a second candidacy walk. As specified, the
filtered half of the claim should not be built. *Fix:* re-derive §5 and §8 against `artifacts_frame`,
then either let the proposal own an elided-row schema — which also disposes of F2 — or delete the
filtered half.

### F2 — UX · defect · §5's identity-and-bit rows

**An elided row is indistinguishable from a real one, and it fails silently.** Contracts §3.2 and
`payload.rs` both carry a load-bearing rule: a null geometry column means *this artifact's layer
declares no such property*, and never *withheld*
([decision 0076](../../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)). An elided row
is a third reading — *you already hold this* — with nothing on the wire to tell them apart. A caller
reads `centroid == null`, concludes the layer declares no centroid, and draws nothing; the same
happens to our own client if its store was dropped between forming the claim and the response
landing. It is also a breach of the proposal's own §1: *smaller, never different in kind*. §5's field
list also omits `masked_count`, which the frame cannot omit. *Fix:* give elided rows their own frame
kind carrying `(layer, id, level, matched)`, which fixes the ambiguity and delivers the 13.6 B/row
row at the same time.

### F3 — UX · defect · §4's mode 1

**Mode 1 is not a projection of mode 2, and the claim that it needs no register pass rests on that
word.** A *caller* dropping false-bit rows leaves the rest of the response intact; a *server* doing
it does not:

- `parent_id` is contracted as naming a parent **only where it is in the same response** (contracts
  §3.2). Drop the unmatched rows and a matched child's parent dangles —
  `clients/ts/core/src/artifactChannel.ts::servedLineage` reads an unresolvable parent as a **root**,
  so the tree reshapes with no error.
- the points frame's `membership:<layer>` column is contracted to carry "always an identifier present
  in the same response's artifacts frame". Dropped rows leave it naming absent ids; nulling them
  instead changes what null means.

*Scenario:* a viewer filters a treed clustering, matched sub-clusters are served and their unmatched
parents are not, every survivor draws as a root, the level picker collapses, and the map looks
plausible. *Fix:* delete mode 1 — a caller really can do the row-dropping themselves, and that
version has none of these problems. Keeping it would need a keep-rule (matched, **or** named as
parent by a kept row, **or** named by the membership column) which no caller can compute, and which
does need the pass §4 says it does not.

### F4 — simplicity, UX · defect · §5's "claim only cells you asked for unbudgeted"

**The rule makes the claim unusable by the only client that exists.** `ArtifactChannel` sends
`artifactBudget: artifactBudgetFor(view.zoom)` on every request and that function always returns a
number — 48 at zoom 0, up to 2,048. There is no unbudgeted path, so the shipped client can never
claim anything, and §9 lists the claim as open without naming the client change it presupposes.
*Fix:* say so in §5, and say which of the two gives way.

### F5 — UX · defect · §5's budget rule with §6.2's union rule

**`prune_children` breaks the union, and only `artifact_budget` is handled.** The budget rule is
handed to the client because the budget is the client's parameter; `prune_children` is the *layer's*
declaration, and `cut()` applies it with no budget at all (`cut.rs`: `budget = None` →
`Plan::new(lineage, passing, prune).serve_at(u32::MAX)`).

*Scenario*, treed layer, pruning on, unbudgeted throughout: over a small box C the server serves
parent P, child K having no visible member there. Over the wider box B both pass and the cut drops P.
The client claims C, the server elides what C would have served, and §6.2 has the client draw
*held ∪ response* = {P, K} where the server's answer for B was {K}. The parent's shape is drawn over
its child with the parent's masked count beside it — the failure `cut.rs` and
[0087](../../decisions/0087-cross-level-edges-are-information-not-rollup.md) exist to prevent, and it
looks like the cache working. §6.3's "never let a claim widen an answer" is true and does not help:
the widening is on the client. Compounding it, §5 explicitly permits a claim naming cells the client
never asked about, which is the same defect with a simpler trigger. *Fix:* require the claimed cells
to be a subset of the request's, and forbid claims on a layer declaring `prune_children`, which the
server knows and publishes.

### F6 — simplicity · preference, with teeth · §5 as a whole

**The alternative that gets most of the saving with no protocol at all is never named.** What a
claim saves server-side is `derived::compute` over the artifact's visible members, plus the record
read for supplied content and the store lookup. That output is a pure function of
`(artifact, M_auth, generation)` — the same three things the content key hashes, and the same fact
the map's §2 uses to justify the *client* cache. A server-side memo on those keys saves the same CPU
for **every** caller including `curl`, with no wire field, no client obligation, no leak-register
reasoning and no honesty required of the client. The map's §3 already flags the gap: *"⊘ Nothing
caches derived geometry"*. The claim's marginal value over that memo is wire bytes alone — F1 prices
them at 22%. *Fix:* make the memo the baseline §5 argues against.

### F7 — performance · defect (evidence) · §8's second ⊘

**The server-cost figure is misattributed and its direction is missing.** The 8–905 ms range is
`artifact-serving-at-scale.md` §7.1's synthetic grid at 10⁸ points / 10⁷ artifacts — a different
corpus, scale and campaign from the response-volume memo the sentence credits. That grid also
declares its own exclusions (verdict and cut only; no gather, no record reads, no wire encoding),
which happens to be the right slice for pricing a second walk, though no reader could tell. And a
claim's cells **accumulate** across a session, so the second walk sits at the broad end of the grid
(553–905 ms) rather than at a random cell; on the scattered layer candidacy returns the whole layer
whatever the viewport, so the second walk is the same full walk as the first. *Server candidacy
roughly doubles to save 22% of the bytes.* *Fix:* cite the right campaign with its exclusions, and
state that a claim's cells are a superset of a request's.

### F8 — simplicity · defect · §5's "the point path's own idiom"

**The borrowed idiom is unbuilt, cited to the wrong section, and inverted on the axis that matters.**
[`delta-serving.md`](../../design/delta-serving.md) is Provisional r2 under review and says the
declarations operand **does not exist**, so the artifact claim would be the first implementation and
inherits none of that document's review —
[0013](../../decisions/0013-mark-specified-vs-implemented.md) requires that marked at the claim,
where "the point path's own idiom" instead reads as reuse of something proven. Declared tiles are
its §3 and §6, not §2. And §2 states the **opposite** rule: *"declarations are ignored on any request
carrying a filter"*, because a filter changes which held items match and letting the client decide
that is client-derived membership. The inversion may well be right now that 0104 has the server
answer the filter, but it must be argued at the site. One thing to carry over rather than drop:
delta-serving's elision reports how many it withheld — the artifact claim must report **no** elided
count, or a caller can probe with it.

### F9 — performance, UX · risk · §5's content-key paragraph

**The claim may be inert exactly where the corpus is live.** A claim under a rotated content key is
ignored and rule 7 drops the store with it. `delta-serving.md` §2 records that the overlay version —
a component of that key — moves on an ingest, before the flush that gives those items rows.
Continuous ingest therefore drops the store and ignores every claim. Points get the softer treatment
(a held band survives a rotation, renderable but stale-marked); artifacts do not. *Fix:* state the
rotation rate as a condition in §5 and as a third ⊘ in §8.

### F10 — simplicity · defect (under-specification) · §6.2's union rule

**The union requires client machinery that does not exist, and the reason is structural.** To compute
*held for the cells claimed*, a client needs per-artifact provenance of the cells each payload
arrived under. `ArtifactChannel` holds `(layer, id) → {artifact, ordinal}` and replaces the served
set wholesale, deliberately (rule 6, trap 5.2); nothing records cells, and the handover has already
rejected the compact wire alternative. Underneath: for points, tiles **partition** row space, so *I
hold tile T* makes the union exact. An artifact belongs to many cells, so the same construction is
not exact for artifacts — which is F5's root as well. *Fix:* say this in §5, since it is the reason
the borrowed idiom does not transfer whole.

### F11 — performance · defect (minor) · §4's mode-3 pricing

**58 KB is 2× low as encoded.** 464,655/8 = 58,082 B is the bit count; Arrow IPC writes a values
buffer *and* a validity buffer, each 64-byte aligned, so the column measures **116,552 B**. The
conclusion is unchanged (116 KB against 6.3 MB is ~54×) and mode 3 in fact looks *better* against
F1's honest denominator, but the figure is quoted as though exact.

### F12 — house style · minor · §8

Unit slip: the response-volume memo measures 49.0 **MiB** and 0.06 **MiB**; §8 writes MB for both.
The 110 B/row model is consistent with the MiB reading, so the model is right and the label is wrong.

### F13 — record · defect · [`../../client-delivery.md`](../../client-delivery.md)

The status record already repeats the wrong figure as the *reason* for the recommendation — "payload
elision saves about 60% of the frame and not 95%". The conclusion survives F1 and is strengthened by
it, but the stated reason does not, so the correction has to land in the proposal, the map's §4a.1
and the status record together or the wrong number will be cited from the record afterwards.

### F14 — simplicity · preference · §9

The map's §4a.4 carried a costed recommendation — *do not build a wire affordance yet* — which the
proposal replaced with a neutral "whether the claim is built … is the owner's". A recommendation the
owner can overrule is worth more than a fourth open question, the more so now that F1 strengthens it.

---

## What the review checked and found sound

§3's account of the built surface matches the code: layers intersected and never unioned, `levels`
defaulting to the declared map, the budget structural rather than a sample, and `matched` a nullable
bit-packed boolean positional to the rows it travels with. §7's disclosure reasoning holds as far as
it goes — mode 1 and the claim both return a subset of what the same caller would otherwise receive,
a false claim receives strictly less, and an elision cannot widen an answer. The measured figures §8
credits to the response-volume memo are in it. The document is indexed with the right status and
`check-doc-links.py` is clean.

## Verdict

**Rework §5; delete mode 1 rather than edit it; §§1–3, 6.1 and 7 are promotable close to as
written**, and mode 3's deferral is right and gets more right once F11 corrects its denominator. §5
rests on a byte model wrong by 2.4× (F1), specifies a wire shape colliding with a rule 0076 makes
load-bearing (F2), cannot be used by the only client without a change nobody had noticed (F4),
composes wrongly with a layer flag it does not mention (F5), and never measures itself against the
no-protocol alternative that gets most of the same saving (F6).

**The single most valuable change** is to re-derive the byte model against `artifacts_frame` and put
an honest elided-row shape in the proposal: that one number decides mode 1, the claim, mode 3's
deferral and the dictionary-encoding priority together. As measured it says the filtered claim should
not be built, and that the change the document mentions only in passing is the real win —
dictionary-encoding `layer` is ~14% of **every** ordinary response (125.0 → ~107 B/row measured), and
nulling the two hull columns when no served layer declares a hull is another ~7% (both together:
125.0 → 98.8 B/row). Neither needs a protocol change, a client obligation or a register pass, and
both help `curl` and the TypeScript client identically.
