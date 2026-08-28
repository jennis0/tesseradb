# Artifact fetch: the questions, the modes, and what each side owes

**Status:** **Proposal — nothing in §4 or §5 is built, and the shape is the owner's to rule on**
(2026-08-28). §3 describes the surface as it stands today, which *is* built. A conflict with
[`contracts.md`](contracts.md) is resolved in its favour until this is promoted.

**Owns:** how a client asks for artifacts and what comes back — the scope of the question, the three
filter modes, and the cache claim — together with what the client must do to ask honestly, what the
server must do to answer safely, and what a caller who has never read this document is still
guaranteed.

**Does not own:** what an artifact *is* ([`annotations.md`](annotations.md)), how it is stored and
evaluated ([`annotation-representation.md`](annotation-representation.md),
[`artifact-serving-at-scale.md`](artifact-serving-at-scale.md)), or the byte schema of the frames
([`contracts.md`](contracts.md) §3.2, which stays the contract).

**Reads with:** [`client-obligations.md`](client-obligations.md) rules 6 and 7,
[`delta-serving.md`](delta-serving.md) §2 (the declared-tiles form this borrows),
[`artifact-cache-handover.md`](../artifact-cache-handover.md) (the work's map and its traps), and
decisions [0083](../decisions/0083-the-frontier-is-a-request-time-budget.md),
[0087](../decisions/0087-cross-level-edges-are-information-not-rollup.md),
[0096](../decisions/0096-layers-are-usually-one-and-the-picker-offers-the-closure.md),
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

Two consequences are load-bearing and are stated at their sites below: the server keeps **no
per-client state**, and a claim the client gets wrong costs a round trip rather than a wrong map.

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

Built, and the baseline every proposal below is measured against.

- **`layers`** names the layers to answer for; omitted or `[]` is none, `"all"` is every reachable
  layer, an array is intersected with the reachable set and never unioned (contracts §3.2).
- **`levels`** names the rungs; absent follows the layer's declared zoom→level map against the
  request's depth (decision 0103).
- **`artifact_budget`** cuts a nested layer's response structurally — ancestors in place of their
  descendants, never by sampling (decision 0083).
- **The *artifacts* frame** carries one row per served artifact: layer, id, key, masked count,
  geometry, content, parent, level, and `matched` — the filter bit, null where the request carried
  no filter (decision 0104).
- **`matched` is an Arrow boolean column**, bit-packed on the wire, positional to the rows in the
  frame it sits in. There is no addressing problem because the bits and the rows travel together.

**The client side** holds every payload it has been served under one identity key and content key,
keyed `(layer, tessera_id)`, and takes the bit from each response rather than from the store
(`clients/ts/core/src/artifactChannel.ts`; `artifact-cache-handover.md` step `cache 2`). It does not
yet tell the server anything about what it holds, so the server sends everything every time.

## 4. The three filter modes

⊘ **Modes 1 and 3 are proposals; mode 2 is what ships.**

**Mode 2 — every artifact in view, each with its bit.** The frame as §3 describes it. Self-describing
and stateless: every row carries its own identity and its own answer, and nothing about it depends
on a previous exchange. **This is the default and it stays the default.**

**Mode 1 — only the artifacts meeting the filter.** The same frame with the unmatched rows omitted.
It is a *projection of mode 2* that the caller could compute themselves by dropping rows whose bit is
false, so it discloses nothing mode 2 did not and needs no register pass of its own. What it buys is
bytes on a selective filter, which is the common one. Existence and the masked count are unchanged —
they stay anchored on `M_auth`, so no artifact appears or disappears because of a filter (**I3**,
**I12**); what changes is only which rows the caller asked to be sent.

⊘ **Mode 3 — the bits alone, with no rows — is deferred, deliberately.** It is the only shape whose
answer cannot be read on its own: bits with no rows must be positional to a set the caller
reconstructed exactly, which forces a contractual response order, a digest to catch misalignment,
and a consumer that has faithfully replicated a set the server computed earlier. Priced on the path
it exists for — view unmoved, filter changed — it is 58 KB of bits against 5.5 MB of identity rows
at the 464,655-artifact layer, and against a few hundred bytes at a layer of a few thousand. **The
case that justifies it is the case with no corpus behind it** (`artifact-cache-handover.md` §6 step
4), so it waits for one. Recorded here rather than dropped, with what it would cost:

- Response order becomes contract — ascending `(layer, tessera_id)`, which both sides can produce
  independently and which discloses nothing about an artifact's position in its level, unlike the
  walk order the frame carries today. That narrows
  [decision 0030](../decisions/0030-determinism-is-not-a-guarantee.md) for one frame.
- The response carries the count and a 64-bit digest of the ordered identifiers the bits are
  positional to; a client whose own digest disagrees **discards the bits and re-asks in mode 2**.
  Misalignment by one otherwise highlights the wrong clusters with no error anywhere.
- Roaring is the encoding to reach for if 58 KB is what hurts — a couple of kilobytes on a selective
  filter — at the cost of a reader in the TypeScript client, which does not exist today.

## 5. The cache claim

⊘ **Proposed; nothing is built.**

**The client says what it holds; the server decides what to send.** The claim is the point path's own
idiom (`delta-serving.md` §2's declared tiles) applied to artifacts:

> *I hold the artifacts for these cells, at this zoom, for these levels, at this content key.*

**Four things travel with it, and each is there because leaving it out elides something the client
never received.**

- **Cells and zoom** — what the claim is about.
- **The levels** — what a cell's answer contained depends on the `levels` the request named, so the
  same cells at a different level are a different claim.
- **The content key** — what the client holds was true under one generation and one visible set. A
  claim under a key that has rotated is **ignored, not honoured**: the server answers in full and the
  client drops what it held. That is the fail-closed direction and it costs a response.

**`artifact_budget` and the claim do not compose, and the rule is the client's.** A budgeted request
is cut structurally, so the client holds fewer artifacts than its cells imply. The budget is the
*client's own* parameter, so the rule is simply: **claim only cells you asked for unbudgeted.** No
server mechanism is needed for this, and none is proposed.

**What the server does with it** is recompute — never trust. It resolves the candidate set for the
claimed cells exactly as it would have for a request naming them, and subtracts. It stores nothing,
remembers nothing between requests, and a claim naming cells the client never asked about produces a
smaller answer for that client and no effect on anyone.

**What may be elided depends on whether a filter is in play**, and this is the one asymmetry:

- **No filter** — the whole row goes. The client's held set for the claimed cells *is* the candidate
  set for them, so it reconstructs the in-view set as *held ∪ response*.
- **With a filter** — the row's *payload* goes and its identity does not. A held artifact still needs
  its bit, and the client cannot derive one: its points are a sample of the matches
  (`client-interaction.md` §2). So held artifacts return as **identity-and-bit rows** — id, layer,
  level, `matched` — which is about 12 bytes with `layer` dictionary-encoded and about 40 without.

**Dictionary-encoding `layer`** is worth doing independently of all of this: it is a repeated string
on every row today, and it is most of what an identity row costs.

## 6. What each side owes

### 6.1 The API user — what is guaranteed without reading any of this

**A request naming a layer and a box gets every artifact that layer serves for that view, each row
carrying its own identity, count, geometry, content, parent and level.** No ordering is assumed, no
prior exchange is referenced, and no state is established. Adding a filter adds one column. That is
the whole contract, and it is what `contracts.md` §3.2 and the OpenAPI description already state.

Every mechanism in §4 and §5 is a narrowing the caller opts into. Declining all of them is not a
degraded mode: it is the specified answer, and the only cost is bytes.

**What a caller must not assume**: the order of rows within the frame (decision 0030 — and mode 3 is
deferred precisely so this stays true), that two requests over the same box return the same set when
the content key has moved between them, or that an absent artifact carries a reason. Absence is one
answer with several causes, deliberately.

### 6.2 The client

Beyond the rules already written in [`client-obligations.md`](client-obligations.md):

- **Claim only what you hold in full for a cell.** That means: asked unbudgeted, at the levels named
  in the claim, under the content key named in the claim.
- **Drop the store when either key rotates**, which rule 7 already requires — and after that, claim
  nothing until the new answers arrive.
- **Take the bit from the response, never from the store.** It is the one field a filter moves
  (decision 0104), and holding it answers this filter's question with the last one's.
- **A merged set is scoped to this request's cells and never to history.** Trap 5.2 of the handover
  forbids accumulating served sets across viewports and presenting the union as this view's answer;
  what the claim licenses is narrower and exact — the union of *held for the cells claimed in this
  request* with *the rows this response carried*. Anything else draws clusters for ground the viewer
  has left.

**The fetch model is the client's own and needs nothing from the wire.** Asking per settled view,
or asking once for a level over the whole extent and picking locally from what it holds, are both
expressible today (§2), and the choice between them is a policy question about response size that a
client makes by observation — there is no cardinality hint and none is proposed (S5, declined
2026-08-25, because a layer's artifact count is a corpus-wide count over objects the principal may
not individually see). **Local picking draws the same set the server would have named**, plus
occasionally the edge of a shape large enough to cross the viewport while its visible members lie
outside it — which is a boundary that ought to be drawn, since the geometry describes the whole
visible membership and never claimed to describe the part in view.

### 6.3 The server

- **Recompute the claim, never trust it.** The claimed cells are resolved through the same candidacy
  the request itself takes, so an elision is exactly what a request for those cells would have
  served.
- **Ignore a claim under a rotated content key**, and answer in full.
- **Keep nothing between requests.** No client registry, no served-set memory, no session-scoped
  cache of what was sent. Everything a response depends on is in the request, the mask and the
  generation — which is what makes a response reasonable about on its own, and what keeps the leak
  register enumerable.
- **Never let a claim widen an answer.** A claim can only remove rows the caller says they already
  have; it can never cause an artifact to be served that a plain request would have withheld.

## 7. Disclosure

**No leak-register row is proposed, and here is the reasoning for each piece.** Appendix C's
inclusion test is that a row exists only where a viewer, reading responses they are entitled to, can
end up knowing something about data they were **not** served; data the service serves never
qualifies.

- **Mode 1** returns a subset of what mode 2 returns to the same caller, computable by that caller
  from mode 2's own bits. Strictly less.
- **The claim** removes rows. A caller who claims what they do not hold receives *less*, learns
  nothing about what they claimed, and gets no signal distinguishing "you already hold this" from
  "nothing was served" — the response carries the rows it carries.
- **Identity-and-bit rows** carry the identifier and the bit for artifacts the caller has already
  been served under the same keys. Nothing new crosses.
- **Existence and the masked count remain anchored on `M_auth`** through all of it. A filter narrows
  what is *sent*, never what exists or what is counted (**I3**, **I12**), and the level and the
  budget are request bounds over artifacts that each passed their own criterion (decisions 0080,
  0083, 0103).
- ⊘ **Mode 3's contractual ordering would need its own look** if it is ever built: ascending
  `tessera_id` is chosen precisely because it is order-free with respect to ordinals, which C8 keeps
  off the wire — but it is a new ordering guarantee and should be argued at the time rather than
  inherited from here.

## 8. Cost, and what is not measured

- **Measured**: 49.0 MB and 1,887 ms for 464,655 artifacts at the whole extent under a full
  principal; 0.06 MB at zoom 10 (`../evidence/memos/2026-08-28-artifact-response-volume.md` §1).
  The declared zoom→level map now bounds that case to 254 artifacts at zoom 0 (decision 0103).
- **Modelled, not measured**: about 110 bytes an artifact for a full row, of which about 40 is
  identity as the frame stands today and about 12 with `layer` dictionary-encoded. Every figure in
  §4 and §5 derived from these is modelled.
- ⊘ **Unmeasured**: the scattered flat layer that gives §2's bottom-right cell its teeth. No corpus
  declares one at scale; an attribute layer over GeoNames' 231,645 `admin4` values would produce
  one, and that is a corpus change rather than a code change.
- ⊘ **Unmeasured**: what the claim costs the server. It is a second candidacy walk over the claimed
  cells, which the response-volume campaign priced for a *request's* cells at 8–905 ms across its
  grid, but a claim's cells are not a request's and nothing has been run.

## 9. What is decided, and what is not

**Decided and built** (2026-08-28): the filter bit and its shape (0104); the level selector and the
declared map as its default (0103); the client's payload store and its drop rules (`cache 2`).

⊘ **Open, and each is the owner's**: whether mode 1 is built; whether the claim is built and in the
shape §5 gives it; whether mode 3 is deferred as §4 recommends; and whether `layer` is
dictionary-encoded now or with the first of the above. Nothing here is implied by anything already
shipped — the surface as it stands is complete and correct, and every item above is a narrowing that
buys bytes.

## Appendix R

**r1 — 2026-08-28.** Written after the owner asked for the whole artifact-fetch space in one place,
following the two steps that landed that day (the filter bit; the client's payload store). Its shape
is the owner's own: the four questions, the three filter modes, and the claim borrowed from the point
path. Three things were settled while writing it and are marked at their sites — that a bits-only
mode forces a contractual order and a digest, and is therefore deferred; that the claim and
`artifact_budget` do not compose, with the rule falling to the client because the budget is the
client's parameter; and that a filtered claim may elide payloads but never identities, because a held
artifact still needs its bit and cannot derive one.
