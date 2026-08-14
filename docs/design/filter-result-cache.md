# The filter clause cache

**Status:** Provisional — the shape is the owner's (2026-08-14); nothing is built against it yet.
What remains: one independent adversarial review — §3's overlay handling and §5's dedup are the two
places where getting it wrong is silent — and the measurement §2 names, which is the one number that
could still say the unit is wrong. Nothing else in the corpus depends on this document.

**Reads with:** [`filter-index.md`](filter-index.md) §2.2 (the mask is the scan's candidate) and §5
(a buffered entity matches no predicate — §3 depends on that ruling),
[`filter-surface.md`](filter-surface.md) §4 (a **superseded** shared cache — read the ⊘ block before
this one), [`caching.md`](caching.md), and `architecture.md` Appendix C rows C24–C26.

## 1. The one argument for it

A filter's result is computed in **entity space, before the tile sweep**, and only then crossed into
row space using the request's own ranges. So the expensive half is **viewport-independent**: a
viewer who applies a filter and then pans or zooms re-evaluates the identical expression on every
frame, and every frame reads the same postings and scans the same columns to reach the same bitmap.

That is the argument, and it is the *product's* rather than the general one. "Queries repeat" is
weak and was rejected once already. "The interaction this system exists for re-asks one question per
frame while changing only the part the answer does not depend on" is not.

**Everything below is subordinate to that.** A cache that does not serve the pan-and-zoom case is
not worth its key.

## 2. The unit is a top-level clause

**Each top-level clause of the request's filter is cached separately** — the category clause and the
text clause of `all_of[{department: eq}, {abstract: match}]` are two entries, and a query reusing
either gets that half free. A clause here is whatever sits directly under the root combinator,
whether that is a single leaf or a nested subtree; nothing below the top level is a cache unit.

**The consequence, which is the cost of the choice.** `eval`'s `AllOf` arm carries one running set
and hands it down — `live = eval(kid, &live)` — so today each clause is evaluated against whatever
its predecessors left standing, and what it computes is *not* that clause's own answer. A cacheable
clause must instead be evaluated against the session's visible set, independently of its siblings,
with the results combined afterwards. So this trades the **sibling** narrowing for reuse.

Two things that trade does *not* touch, and both were nearly confused with it:

- **Narrowing inside a clause is unaffected.** A multi-word `match` still narrows token by token
  from the candidate, never materialising a corpus-wide posting
  (`probes/2026-08-14-hidden-vs-absent/`, and `ColumnPostings::narrow` intersects against the mapped
  file). That mechanism lives inside one leaf, and it is the one the 32–61% measurement is about.
  A cached clause's value was produced *by* it.
- **Nesting is unaffected.** Clauses inside a top-level subtree still narrow against each other,
  because the subtree is one cache unit.

⊘ **What the sibling narrowing is worth has never been measured.** It arrived with the boolean tree
(2026-08-09) and no probe has isolated it. The number that would settle whether this unit is the
right one is: one selective clause and one broad clause, evaluated narrowed against evaluated
independently, at a few coverage levels. **If the loss is large, the alternative is the whole
expression as one unit** — which serves the pan case in §1 just as well, holds the smallest set in
the evaluation, and gives up cross-query reuse entirely.

⊘ **The entity route only.** Decision 0068's row route evaluates a leaf over the request's own rows,
which are viewport-dependent, and `evaluate_routed` chooses between the two per request on
`rows_in_ranges ≤ v_total` — so the same filter takes different routes on different frames. Only
`RoutedFilter::Entity` is cacheable. A frame that takes the row route does not hit, and it took that
route because it was cheap for that frame.

## 3. The key, and why the overlay is not in it

The candidate a clause is evaluated under is `filter::candidate`:

> `fragment.view() ∖ overlay.denied()`, plus every buffered entity whose verdict admits it

**A cached clause is evaluated against `fragment.view()` alone** — before the deny state, and
without the buffer — so the key is:

| component | why |
|---|---|
| the canonical clause (§5) | the question asked |
| the **session's `token_id`** | **an entry is never shared between sessions** (ruled 2026-08-14) |
| `segments_version` **and** `prefix` | a flush publishes entities into the index, which genuinely changes what a clause matches. `RowProjectionKey`'s doc argues why the prefix is not redundant with the version within a process |

**Sharing an entry between principals was declined, and the reason is not that it could not be made
sound.** Two sessions holding the same grants ought to have the same visible set, and the version
machinery does guarantee that much: `check_publishable` refuses a publication that does not strictly
increase `segments_version` or that regresses the watermark, and both filter entry points reach the
fragment through `Engine::fragment_for`, which rebuilds a stale one rather than serving it. What
declines it is two things the review found:

- **It is false today.** [#112](https://github.com/jennis0/tessera-index/issues/112) records two
  sessions with identical satisfied term sets served different visible sets at the same instant,
  reproduced four times. The mechanism is still a hypothesis, so the direction it can go is unknown.
- **It creates a channel nothing in Appendix C covers.** Two principals in one compartment sharing
  entries lets either time a clause against the other — a hit is microseconds, a miss is a scan
  measured in tens of milliseconds to seconds — and read off which queries the other has recently
  run. C24 and C25 are corpus-content oracles bounded to strings the caller already holds, and are
  accepted partly because the revealing case is nanoseconds wide. This one is remotely readable and
  is about another *viewer*, not about the corpus.

And it buys nothing §1 asks for: one viewer panning is served completely by a per-session key, which
is what `RowProjectionKey` already does for its own reason. Sharing would be a second design, with
its own register row, bought for a case this document does not make.

### The deny state is subtracted, not keyed

Every request applies `∖ overlay.denied()` to the composed result. The algebra is exact —
`f ∩ (view ∖ D) = (f ∩ view) ∖ D` — and set difference commutes with itself, so the negation path
(`present ∖ matched`) is equally safe under it.

**This is the fail-closed arrangement, and the keyed alternative is the fail-open one.** Folding
`overlay_version` into the key means a suppression is honoured only because somebody remembered a
field; omit it and the cache keeps serving a suppressed item silently, presenting as an improved hit
rate — the fail-open Rule S exists to prevent (write-path §5.4), reached by a new route. As a
subtraction it is a step in a request path instead, and **the cached value must be a type that
cannot be read without applying it**. That obligation is this design's, not a reviewer's to notice.

It is also cheaper than what happens now: the same `andnot` is already performed per request, over
the whole visible set. Here it is performed over the result, which is smaller.

⊘ **The cache therefore holds sets containing that principal's suppressed and deleted-but-unfolded
items.** They are inside `M_auth` — suppression is an overlay above the fragment, not a narrowing of
it — so this is not cross-principal reuse. It is still a set held across requests that no request may
return, and it is named here because a reviewer should look at it rather than infer it.

### An ingest does not invalidate, and that is a ruling elsewhere

`Generation::overlay_version` moves on every buffer swap as well as every overlay swap, so keying on
it would throw an entry away on every applied ingest batch. **Those invalidations would all be
false.** A buffered entity is in the candidate and in no index layer, so it matches no predicate:
filters under-report freshly ingested items until their flush, which `filter-index.md` §5 rules on
directly and I12 makes safe. Including the buffer in the candidate cannot change a clause's answer,
so excluding it from the key changes nothing.

**⊘ That is a coupling, and it is the one a future change would break silently.** If entity-space
filters ever see buffered entities, the buffer returns to the candidate *and* to this key. A cache
left as written would then answer from a set that predates the ingest, with no error anywhere.
Anything revisiting that ruling must revisit this.

## 4. Why this is not the cache that was superseded

`filter-surface.md` §4.1–§4.5 specify a **shared projection cache** and are marked *do not
implement*. Its premise was that an operand is evaluated **unmasked**, which is what made a result
principal-independent and shareable. That premise has been false since `filter-index.md` §2.2 made
the mask the scan's own candidate.

This proposal shares across principals only where their **term sets are identical**, which is the
same-visible-set case and is sound for that reason alone. The two look alike and differ exactly
there, so the distinction is stated rather than left to be noticed.

## 5. Canonicalisation, and the dedup it buys

**Identical clauses are collapsed before evaluation.** A request may name a clause any number of
times — breadth is unbounded where depth is capped at four — and five hundred copies of one filter
must cost one evaluation, not five hundred. Deduplication is sound on every combinator this surface
has, each being idempotent: `A ∩ A = A`, `A ∪ A = A`, and `out ∖ A ∖ A = out ∖ A`.

**The canonical form the cache keys on is the same one the dedup compares**, which is why there is
one mechanism here and not two: a clause is canonicalised to its column, its operator, and the
**identity the operator's own semantics give its operand after analysis**. What differs per family
is only what that identity is.

- **A text `match`'s identity is a set** — the sorted, deduplicated token list, plus
  `minimum_should_match`. So `"The Archive"` and `"archive the"` are one clause and one entry,
  because the operand already deduplicates its tokens and ignores their order; the canonical form
  collapses exactly what the engine would have answered identically anyway.
- **A `phrase`'s identity is the whole ordered token sequence, taken as one term.** A phrase is not
  a compound of its words — it is an atom whose surface form happens to contain spaces — so it
  cannot be reordered, deduplicated or decomposed, and two phrases sharing every word share no
  entry. Borrowing `match`'s canonical form would make `"quiet harbour"` and `"harbour quiet"` one
  clause and answer one of them wrongly.
- **The two identities meet in exactly one place, and it should collapse rather than duplicate.** A
  one-token phrase *is* a `match`: the engine returns the conjunction without reading the record
  blob at all below two tokens, adjacency having nothing to constrain. So `phrase "harbour"` and
  `match "harbour"` are one predicate and must key to one entry, not two entries holding the same
  set.
- **Every other family canonicalises structurally** — column, operator, and the operand's values in
  a fixed order. Do **not** attempt to prove that two spellings of a boolean tree are equal:
  missing an equality costs hit rate, and getting one wrong costs correctness.

⊘ **What atomicity costs, stated so it is a choice rather than an oversight.** A phrase's answer is
a subset of its own words' conjunction, and `verify_phrase` computes that conjunction internally
before reading a blob row per survivor. An opaque phrase identity means a cached `match "quiet
harbour"` cannot be used to start `phrase "quiet harbour"` at its verify step. That relationship is
real and is deliberately not exploited here: it would make one entry's validity depend on another's,
which is a second invalidation rule for a saving nobody has measured.

Analysis is already performed per clause inside `resolve`, so canonicalising early moves that work
rather than adding it — and it happens in the engine, where the column's declared analyser is
resolved, not at the parse gate, which cannot see one.

⊘ **Dedup bounds repetition, not breadth.** Five hundred *different* clauses still cost five hundred
evaluations, so this is not a substitute for the node count checked at parse
([#121](https://github.com/jennis0/tessera-index/issues/121)) — it removes the cheapest abusive
shape, and the cap is what bounds the rest.

## 6. What to reuse

**Reuse `SingleFlightCache` (`engine/src/single_flight.rs`), and model the wrapper on
`RowProjectionCache` (`engine/src/cache.rs`).** It already carries the byte bound, LRU eviction, the
single-flight state machine — without which N concurrent identical requests all miss and all compute
— and two pruners for entries whose generation no request can name. `cache.rs`'s module doc and
`RowProjectionKey`'s field-by-field argument are the model for what the new key's doc owes,
**including the note on why the key is a struct with named fields rather than a tuple of `u64`s**: a
transposition there is cross-principal mask reuse that compiles and runs.

**Seam:** the cache sits above `FilterColumns::resolve` and below `eval`, and needs the key's
generation components threaded down to a function that does not see them today — they are on the
`Generation`. That threading is the bulk of the work and is worth designing before typing.

## 7. The tests that would make it safe

Each names its mutation, because a cache test that cannot fail on a weakened implementation proves
nothing:

- **A suppression is honoured on a hit.** Filter; suppress a matching item; filter again with the
  same clause and the same principal — the item must be gone, and the second request must have hit
  the cache. *Mutation:* skip the subtraction, and this must fail.
- **An unsuppression restores it**, from the same entry. Rule S's other half: the artefacts were
  never touched, and neither was the cached set.
- **A flush invalidates**, over `segments_version`. *Mutation:* remove it from the key.
- **An ingest does not invalidate, and the answer is right either way** — the entry survives, and
  the freshly ingested item is absent from the filtered answer because it is unflushed, not because
  it was cached away. This is the test that pins §3's coupling.
- **Two principals with the same term set share; two with different term sets do not.**
  `eviction_never_widens_a_mask` in `cache.rs` is the precedent for the second half.
- **Five hundred copies of one clause cost one evaluation**, and a phrase and its reverse cost two.
- **A one-word phrase and that word as a `match` are one entry**, and the same two words as a phrase
  and as a `match` are two — the collapse and the non-collapse asserted together, since a rule that
  did neither and a rule that did both would each pass half of this.

## 8. One channel to register rather than discover

A shared LRU means one term set's traffic evicts another's, so hit-or-miss timing weakly signals
that some other term set is active. This is very weak beside C24 and C25, which are already
accepted — but Appendix C is exhaustive only if new surfaces are named, so building this owes the
register a row rather than an argument that it did not need one.
