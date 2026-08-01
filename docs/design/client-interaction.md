# Client interaction architecture — how anything talks to Tessera

**Date:** 2026-07-31
**Status:** Provisional r2 — under review. Graduated into the corpus 2026-08-01 because code is already written against it; it is not yet normative. r2 applies decisions 0029 (the composite formerly called "epoch" is the **view key**) and 0030 (determinism is documented, not promised) — see §16. **To become normative:** owner sign-off on the protocol surface, and its four children (caching, derived-artifact gating, tile-addressed integration, and the MVP viewer) reconciled against what shipped. Where this document and `architecture.md` disagree, the architecture design governs.
**Succeeds** `../archive/visualisation.md`, which owned *rendering* and is archived; this document owns everything protocol-facing and is the reference for the client that was actually built.
**Touches:** design §2.2–2.6, §7.1–7.5, §8, §11.2, Appendix C, Appendix H; contracts §3, §5; SA §4.2–4.5; plan §5–§9; viz architecture §1–§2, §7, §9.

---

## 1. Why this exists, and what altitude it sits at

The design specifies a service. The visualisation architecture specifies how marks
become pixels. Neither says what a *client* is: what it holds, under which version
coordinates, what it is obliged to display, and what a stranger's system must do to
consume Tessera without adopting our code.

That gap matters now because of an owner goal stated 2026-07-31: **if we want
adoption, building an app on Tessera must be easy, and the right way to use it must
be the obvious way** — across three usage modes (local data-science project; the full
Tessera package with a *buildable* UI; Tessera as a backend behind somebody else's
frontend) and eventually three data shapes (data maps; geographic points; 1-D
timelines).

This document is **principles and architecture, not contracts**. Where it names a
mechanism it names the shape and the reason, and leaves the byte-level statement to
the phase that owns it. Owner ruling, 2026-07-31: *"My question isn't what contracts
should we build — it's what underlying mechanisms and design approaches should we
take."*

**Timing:** design now, build later (owner, 2026-07-31). Nothing here schedules work
except §13.

**This document is deliberately an umbrella and does not decompose into one
implementation plan.** It fixes boundaries and vocabulary across a span — the finished
verb surface, the client stack, the integration tier, the customisable UI, the data-shape
stretches — each of which wants its own spec and plan against §13's gradient. Read its
size as the span it covers rather than as the size of any piece of work in it.

**One companion has already split off.** The gating of *derived artifacts* — clusters,
labels, hulls, boundary polygons, edges — is
**`derived-artifact-gating.md`**. It left on 2026-08-01 because it is not
a client question: it generalises design §7.5 and §7.6, its audience is a security
reviewer, and it carries a live disclosure question that belongs with C1's review rather
than with an integrator. §9.1 keeps the pointer and the three conclusions this document
relies on.

## 2. The anatomy: two channels

Four integration seams were worked end to end (§8). All four are **count-blind** —
including deck.gl, the one seam where we control both sides. That is not a limitation
of anyone's library; it is a property of the boundary between rendering and meaning,
and it gives this document its organising anatomy.

**The mark channel** carries drawable attributes — positions, identities, encodings —
through whatever renderer seam is in play: a deck.gl binary attribute buffer, an MVT
feature, an Arrow record batch. It is high-volume, per-viewport, and fungible across
renderers.

**The number channel** carries masked quantities — `visible`, `matched`, `served`,
region breakdowns, cluster counts, anything with a number in it — through the viewer
verbs into panels, legends and DOM. It is low-volume, exact, and **never** derivable
from the mark channel, because the marks are a sample and the numbers are not.

**"Number" is too narrow, and the derived-artifact work corrects it** *(2026-08-01; see
`derived-artifact-gating.md`)*. A cluster hull is an aggregate over the
visible set, so a client that draws a hull around the *k* points it holds has committed the
sample-as-set error **in geometry** — the same failure in a shape nobody thinks to check.
So this is the **exact masked-aggregate channel**, and geometry travels on it: hulls,
centroids, contour inputs and cell counts alike. The mark channel carries the sample; this
one carries whatever is exact, whatever its shape.

Every seam analysis in §8 falls into this shape: *renderer via the seam, product via
the verbs.* An integration that uses only the mark channel produces a picture with no
trustworthy quantities in it — which is a legitimate product (a viewer) but must be
named as one.

**The obligation this exposes is cross-channel view-key consistency** — but it is smaller
and more mechanical than the split makes it look, and an earlier draft over-dramatised
it *(third review, 2026-08-01)*. Within one response the channels are atomically
consistent **by construction**: the viewport verb delivers tile counts and points in a
single body (contracts §5). The risk is purely *temporal* — composing responses fetched
at different times, so that a cached tile sits beneath a fresh count or the reverse.

So the obligation reduces to **render only responses sharing one view key** (§6), which
is snapshot isolation, and it is therefore a **data-structure property of the replica
store** rather than a discipline every integrator must hold: all state keyed by view key,
the renderer reading exactly one view key's keyspace, flips atomic. That reduction is what
makes it testable — the conformance assertion is one predicate, *no frame mixed
view keys*, rather than a review of everything a client draws. Only a client can enforce
it: the service answers one request at a time and cannot see the screen.

The two-channel split remains the document's anatomy. It is *not* the source of the
consistency problem, and reading it as such invites a heavier mechanism than the
problem needs.

## 3. The lifecycle: a client is a versioned partial replica

Appendix H writes the first sentence of this: *"Tessera is a materialised per-viewer
selection."* Extended across the boundary — **a client is a versioned partial replica
of the viewer's materialised selection.**

Every interaction is *extend the replica* (pan, zoom, drill down) or *refresh it*
(filter change, re-authorise, change signal). Caches are replicas. §5's advisory
reconciliation is the replica's extension protocol. The change signal is its version.
The consistency rules of §4 are the replica's invariants.

This is chosen over "verbs and filters" as the organising metaphor for two reasons.
It subsumes it — the verbs are *how* a replica extends — and it is teachable: an
integrator already knows what a replica with a staleness bound is, and knows to ask
what voids it. Prior art an integrator will recognise: Replicache, LiveGraph, and
CDC-into-a-materialised-store, which Appendix D notes is what the most sophisticated
authorisation service in the survey eventually became.

## 4. Principles

Six. Each is stated with the failure it prevents, because most of them read as style
advice until the failure is visible.

**P1 — The client renders masked quantities; it never computes them.** Counts,
densities, cluster sizes and label visibility come from the number channel. *Prevents:*
a figure derived from the sample, which is wrong, and indistinguishable from a correct
one in a screenshot. *Bounded deliberately:* the client computes plenty — hit testing,
tile arithmetic, live lasso highlight, colour and size mappings (§9). The prohibition
is on **masked quantities**, not on computation.

**P2 — The sample must never be able to masquerade as the set.** Any surface presenting
a selection, an export or a summary carries both numbers. *Prevents:* confidently
wrong downstream analysis — the failure the viz architecture already rates worse than
a slow query. *Mechanically:* every set-like value in a client API is a type carrying
`{shown, total}` inseparably, so a custom panel cannot render a bare sample count
without deliberately destructuring it; and the export path (§7) **refuses** above its
threshold rather than silently truncating.

**P3 — Derivability is the default admission path, not a prohibition.** A surface
provably derivable from the enumerated viewer verbs ships on that basis alone. A
non-derivable one is not forbidden — it takes the Appendix C route: its own register
entry, its own I2 argument, its own owner sign-off. Derivability proofs must cover
**error paths and cost profile**, not only payloads; C4's annotation sets the house
standard at work-indistinguishability, not output-equivalence. *Prevents:* the leak
register quietly ceasing to be exhaustive as clients and adapters accumulate.
*Constrains silence, not capability.*

**P4 — Authority lives where the credential lives, on both axes.** `auth_data` is
constructed where the authoritative knowledge is (I6). Two axes, not one: *who holds
the token*, and *who can fabricate the auth data*. *Prevents:* the two defining
anti-patterns of §6.

**P5 — Membership is server-authoritative; client input is advisory.** Any hint a
client sends — held state, cache declarations, prefetch intent — may only elide work.
It may never widen or determine what is served. **A server that ignores every hint
must still be correct.** *Prevents:* the entire class of cache-coherence fail-open
paths, with no coherence protocol to review.

**P6 — The naive path is correct; sophistication is opt-in and lives in the client.**
A consumer that ignores reconciliation, view keys, prefetch and every optimisation gets
correct behaviour, only slower. Nothing a client *must* do to be correct may be
complex. *Prevents:* a REST surface whose reference implementation is a disguised
requirement. *Has teeth:* §7.2's *k*-non-decreasing rule is a correctness-affecting
obligation the server cannot enforce and a naive client will get wrong — under P6 that
is a defect to design out, not an obligation to document. Two mechanisms, because the
replica store is not the only layer: the store owns *k* and never exposes it; and for
the bare session client of §10, which by construction exposes the verbs as they are, the
design-out is already in the contract — §3.2 defaults *k* to the deployment's own
ceiling, so a caller who never mentions *k* can never decrease it. **The obligation
survives only for a caller who both sets *k* explicitly and varies it**, which is a
narrow and self-selected population rather than the default path.

**And the division that governs what conformance is for.** Secrecy is **structural**:
every byte a client holds is inside `M_auth` by server construction (I2, §10.4), so a
misbehaving client cannot disclose another principal's data — it can only mislead its
own user. Truthfulness is **conformance**: what the client is obliged to display
honestly is not enforceable by the service and is the conformance kit's whole subject.
Stated for integrators, this is also a selling point: *a buggy client harms its own
user's picture, never anyone else's data.* Two residues survive the division and are
handled by rule rather than structure — cross-principal **persistence** (a cache
outliving a user switch; §10) and cross-principal **style sharing** (§9).

**On staleness** (owner ruling, 2026-07-31). The boundary is **the request, not the
pixel**. No accepted change may be invisible to the *next* response — lifecycle §2.3,
"a suppression applies to a pinned request the moment it is accepted", which is the
server's behaviour and non-negotiable. What a client has already drawn persists until
it refreshes, bounded by token lifetime, which I6 already makes the caller's to set.
**The server never relies on the client to forget.** One corollary the cache must
respect: a client that answers pans entirely from held tiles makes an accepted change
invisible indefinitely, because there is no next request. *Don't re-download* is free;
*don't re-request* needs an explicit staleness bound, and the bound is the view key (§6).

**The budget is minutes, in both directions** *(owner ruling, 2026-08-01)*: *"minutes of
latency on new items appearing, and minutes of latency on items disappearing, so long as
there's a way to refresh. Staleness is something to be managed, not prevented."* Two
things follow, and the second is a requirement rather than a permission. The client-side
budget is **generous and symmetric** — appearance and disappearance are governed alike,
so §6.1 needs one flip policy rather than a security-bounded one for denies and a relaxed
one for ingest. And **a refresh path must exist and be reachable**, because the whole
ruling rests on it; a client with no way to refresh has converted an accepted staleness
budget into an unbounded one. The affordance is therefore mandatory in the conformance
sense, not a product nicety.

**And client-side disappearance latency is not a security control** *(owner, 2026-08-01:
"we can never take back a served item — so anything on top of that is merely window
dressing… if access changes after something has been read in the past, that's just tough
luck")*. This is the more honest framing and it should govern the whole document. A
served item is served: the disclosure completed at serve time and no client behaviour
retracts it. Redrawing it from cache to the same principal discloses nothing that has not
already been disclosed.

So exactly **two** mechanisms carry the security here, and neither is a timer:

1. **The server never serves it again** once the change is accepted (lifecycle §2.3).
   That is the whole of the authorisation boundary, and it is the server's behaviour.
2. **The cache dies with the principal** — the session owns cache lifetime and drops it
   on token change (§10), which closes cross-principal persistence, the one client-side
   leak the secrecy/truthfulness division leaves standing.

Everything else — flip deadlines, cache TTLs, refresh cadence — is **coherence and
hygiene**. Worth doing so the map does not quietly misrepresent its own currency, not
worth defending as a control, and specifically not worth building machinery for. A
document that presents them as controls invites a reviewer to lean on them, which is how
window dressing becomes load-bearing.

## 5. Reconciliation: what the client already holds

Owner motive (2026-07-31): *"points are stable at a given pan/zoom by construction — so
it's about not restreaming, and more importantly for the backend, not having to
re-read, points the client already has."*

Stability is conditional and the precise form matters, because the protocol depends on
it: `served(viewport)` is stable within **(mask, overlay version, slice, k, idset)**.
§7.2 accepts θ movement on overlay swap; contracts §2.6 makes row order key-dependent.
Those five coordinates are the **view key** (§6). The viewport is not one of them, and
that is the whole point of the concept: a served viewport is stable *across* viewports
within one view key.

**The mechanism.** §7.2 defines `served(T)` as the smallest `m(T)` members of `vis(T)`
by `tessera_id` — the served set is a **`tessera_id`-order prefix** of the visible set
in a range, the union of floor, threshold and cap being a prefix is what the nesting
proof turns on, and the client can evaluate that ordering itself because it holds the
identities. So client state is declarable as **prefix declarations** — *for tile T I
hold everything up to cut c, as of view key E* — rather than identity lists. One parent
declaration answers for all four children on zoom-in, which is the case a tile-level
ETag cannot cover.

Three properties, in the order that matters:

*Fail-closed by construction.* The server names the complete served set; membership is
never inferred client-side. A suppressed item is simply not named, so the client drops
it. No invalidation protocol exists to get wrong.

*Advisory.* A server ignoring every declaration is correct (P5). That is what makes it
reviewable, and it is why the view-key guard suffices for the one real bug: an item
flushed or unsuppressed below the client's cut would otherwise be assumed-held and
arrive with no attributes, so a stale view key simply drops the declaration.

*Opt-in, and self-contained by default.* §8 corrected an over-generalisation here.
Only deepscatter wants deltas on the wire; deck.gl and MVT want self-contained tiles
and Mosaic wants snapshots. So **self-contained responses are the default and elision
is negotiated**, because the dominant tile consumers reassemble per tile and must not
be made to hold reassembly logic. Our core opts in; a stranger never sees a delta.

**Say this in OGC 3D Tiles' vocabulary rather than inventing our own** *(survey,
2026-08-01)*: that standard already names per-tile refinement **`ADD`** (children add to
the parent) versus **`REPLACE`** (children are self-contained). That is exactly this
distinction, standardised, and anyone who has touched streaming geospatial reads it
without explanation. **Tessera serves `REPLACE` by default; `ADD` is negotiated per
session.**

**And the shape is convergent, which is worth claiming precisely.** Three independent
systems arrived at importance-ordered additive point tiling: Potree stores a subsample
per octree node whose union along the root-to-leaf path is the full cloud; HiPS
progressive catalogues (IVOA, astronomy) put the brightest sources at coarse orders and
add fainter ones with depth, a decade ago; quadfeather assigns points to the shallowest
tile with capacity. **The convergence is on the mechanism; what none of them has is a
*defensible order*, because none of them needed one** — theirs are static, global and
single-ordered. §7.2's keyed per-viewer priority prefix is the instance of a shape the
field keeps rediscovering, which is a stronger and more checkable claim than novelty.

**Declarations key on tile identity, not row ranges.** A row range dies at every
compaction (I11); a Morton prefix is deliberately stable across pins.

**What it saves:** the column gather and the wire bytes. Not the `tessera_id` read —
§7.2's comparator reads the full identity during selection regardless.

**Rejected: client-derived membership** (server sends nothing, client computes which
held points qualify). It saves the identity read and is fail-open on newly-suppressed
items. Recorded so it is not re-proposed.

**Annotated 2026-08-01 — this rejection's stated ground contradicts §4, and the contradiction
misleads.** *"Fail-open on newly-suppressed items"* reads as a security verdict. §4's owner ruling
of 2026-08-01, which post-dates this paragraph, says the opposite in terms: *"a served item is
served: the disclosure completed at serve time and no client behaviour retracts it. **Redrawing it
from cache to the same principal discloses nothing that has not already been disclosed.**"* A client
drawing a suppressed item it already holds is **stale, not fail-open** — an inconvenience §4
explicitly accepts, bounded by the view key.

The rejection is **re-grounded, not withdrawn.** What actually survives it:

1. **Reviewability.** The server naming the complete served set is what makes §5 checkable in one
   place; membership inferred client-side has no invalidation protocol to review, which is the
   property this section opens by claiming.
2. **§4's own corollary, which is about bounds rather than disclosure.** *"A client that answers
   pans entirely from held tiles makes an accepted change invisible indefinitely, because there is
   no next request. Don't re-download is free; don't re-request needs an explicit staleness bound,
   and the bound is the view key."* Client-derived membership is the unbounded form; §6.1's
   rule 3 — compare the view key, render stale-marked — is the bound that makes it acceptable.
3. **Removals stay server-authoritative** regardless, from the deny set and the stamp ledger. That
   is the half no client derivation may touch, and it is what the "fail-closed by naming" property
   in this section is really protecting.

**And the trade has moved 20-fold, so the rejection is worth revisiting rather than inherited.** At
the 1–2 × 10⁶ drawn-mark operating point, one interaction is *derivable client-side for free*:
because priority prefixes nest, a zoom-out's served set is a subset of the union of the children
already held, so the client could render it with no request at all — against a measured 8–16 s of
server CPU for a full view at 10⁹. That is the single cheapest interaction available and this
paragraph currently forbids it on a ground that no longer holds. Any revisit must keep (1) and (3)
above and must carry §6.1's view-key bound; it is a coherence design, not a disclosure one.

*Recorded because the "fail-open" wording caused exactly the error it should prevent: it was read,
during the 2026-08-01 caching work, as making stale redraw a leak.*

**Deferred pending a written safety argument: view-key-delta naming** — naming only
changes since the client's view key rather than the complete served set. It fixes the
economics at the large drawn-mark budget, where naming 10⁷ identities costs ~80 MB
with every attribute elided, and the completeness guarantee it needs is one the
contracts already give replicas via the deny set's immediate-publication rule. It also
trades away the fail-closed-by-naming property that makes §5 reviewable, which is
precisely the class of thing the three retirement rules warn gets conflated. Adopt only
with the argument written first, and only if P2's numbers demand it.

### 5.1 Where the replica state lives — the axis this document initially fixed

Everything above treats *who holds the replica record* as settled (the client) and only
*how much we trust it* as variable. **Flipping that axis is the most promising open
direction in this document** *(third review, 2026-08-01)*.

The server already holds per-session state: masks per token, θ per session, the
drawn-mark spec's handle tables. A **per-session cursor** — at minimum the last view key
answered, at most the tile→cut record mirroring what it named — is a small addition to
state that exists anyway, and it changes the safety calculus above. The fail-open risk
in delta naming is trusting a *client's claim* about what it holds; if the server
computes "changes since view key E" from its **own** record — the deny set since E, which
contracts §2.3's side-manifest rule already publishes immediately, plus what became
visible since E — then no client claim is load-bearing and the completeness guarantee
becomes checkable in one place.

Tessera's version of this record is far cheaper than the field's, and for a reason
specific to this design: because `served` is a `tessera_id`-order prefix, `(tile, view key,
cut)` is a *complete* description of client state — three integers, where the sync
engines need a key-to-version map. **The mechanism this document already has is the
compression; §5 merely gives the record to the other party.**

*Prior art, including one nobody has brought.* Replicache's server-held Client View
Record diffs a recomputed authoritative view against what a client was last sent, so
revocation propagates as deletions for free; PowerSync's protocol distinguishes REMOVE
(left your visible set) from DELETE, which is the wire-level echo of the three
retirement rules. **The unclaimed body of evidence is game-server interest
management** — server-authoritative per-client visibility with delta replication,
fog-of-war computed server-side *because clients cheat*. It is the one field whose
security posture matches this one, with two decades of scale evidence behind it, and
Appendix D's survey covers databases and authorisation services only.

*What the cursor is for, corrected* **(owner, 2026-08-01)**. The case above is argued
from denies. That is the *weakest* case — denies are rare and their delta is O(1). The
common case is **ingest**, whose delta is proportional to the flush's share of the
corpus (§6.2), and which is therefore both more frequent and larger. The cursor's value
should be argued from ingest; it survives comfortably even at a 10% flush, where naming
the delta still beats refetching the viewport by an order of magnitude.

*And a caution against overreach from the same review.* Full per-session incremental
view maintenance — the Materialize or Zero posture — is **wrong-sized** here, because
re-answering a tile from the Roaring mask is already O(containers touched), so a refetch
costs nearly what computing the delta would, while the machinery costs real review
surface. Figma's LiveGraph reached this verdict at scale: invalidate-and-refetch beat
incremental maintenance because invalidations on active views are sparse. Take the
session cursor for delta economics at the 10⁷ budget; do not take the layer above it.

## 6. The view key, and the change signal

All four seams independently demand a client-visible unit of cache validity: MVT needs
view-key-scoped URLs for safe browser caching, deepscatter needs reload scoping,
Mosaic-extract needs a consistent snapshot, deck.gl needs an `updateTriggers` key. So
the view key is an **integration requirement**, not an internal optimisation.

The view key is the five coordinates of §5 — mask, overlay version, slice, *k*, idset —
and it is the coordinate within which `served(viewport)` is stable. **The viewport is
not one of its components**, and that exclusion is the concept: a served viewport is
stable *across* viewports, so every pan and every zoom within one view key answers from
one coherent snapshot. A client holds one cache entry per view key and flips atomically
between them, which is why it is a *key* — something that identifies — rather than a
*state*, which would invite a reader to assume the camera was included.

Distinct from `x-tessera-pin`, which is row-space geometry and deliberately not
authorisation state (I11).

**The change signal is the one renderer-relevant primitive that is not derivable from
what the principal may see.** "Something in your view changed" is metadata about corpus
activity beyond the mask. It therefore takes P3's Appendix C route: served, with its
own register entry, of C15's shape (rate of change observable; values per-session
scrambled so nothing correlates across sessions). Everything else in §9 is a product
ruling; this is where the genuine leak analysis concentrates.

Its lineage is worth citing because integrators will recognise it: SpiceDB's
**ZedTokens** and OpenFGA's consistency modes are client-visible consistency tokens
bounding staleness explicitly. No other access-control system in the survey gives its
clients authorisation state at all; the Zanzibar family is the exception and the model.

**The reconcile table** — which signal voids which client state — is the artifact a
mode-3 integrator most needs, and this document owes it: token expiry, pin drain
(`410`), overlay version, idset advance (`409`), and key rotation each void a
different subset of {attribute cache, prefix declarations, held identities, θ
constants, node handles}. Writing it out is a task for the phase that implements the
signal; naming it here is what stops five invalidation paths being discovered one at a
time.

### 6.1 Live data, and what the client does when the signal fires

An earlier draft designed the signal's leak posture carefully and its *behaviour* not at
all *(third review, 2026-08-01)*.

**The cadence is already decided, elsewhere, and it decides the transport.** Design §3
puts the whole write path — denies included — at seconds to minutes, because it is
human-reaction-dominated. So the change signal never needs sub-second delivery: it may
tick on a configured cadence of order seconds, batching view-key advances. That kills by
construction the failure this section should fear — view-key churn under continuous ingest
driving constant refetch — and it means SSE, long-poll and plain polling are all
adequate. SSE is the mild favourite because `Last-Event-ID` gives view-key resume for free
and it is proxy-friendly; nothing here needs bidirectional push. The field agrees: live
layers over tiled maps are universally poll-or-invalidate, never per-tile push.

**Scope the signal per session, which makes the register entry smaller rather than
larger.** A broadcast "the corpus changed" is C15's shape. But the server can intersect
an accepted change's entities against each live session's mask — one `and_cardinality`,
machinery that exists — and signal only sessions whose **own view** moved. The signal
then carries information that session's next §7.1 counts would disclose exactly anyway,
which is C18's accepted argument, and the entry narrows from *corpus activity rate* to
*your-view activity rate*. A residual remains — the timing of *not* being signalled
correlates weakly with others' activity — and still needs the entry, but it is a
strictly smaller channel than the broadcast design.

**Three client rules, which are the reconcile table's behavioural half.**

1. *Refresh is a new view key's snapshot, not an in-place update.* Fetch behind the current
   display and flip atomically when the visible tiles and their counts are complete —
   double buffering, standard in every tile map, and the operational form of §2's
   snapshot isolation.
2. *There is no forced flip, and an earlier draft's deadline was window dressing*
   **(owner, 2026-08-01)**. A deadline that force-flips mid-read buys nothing — the
   served item was already disclosed (§4) — and costs the user their place. What replaces
   it is cheaper and honest: **mark the staleness accurately and keep refresh reachable.**
   The map says what it is as of, the user refreshes when they choose, and the flip
   happens on the next pan anyway, when the layout is already changing and §6.2's churn
   is masked. **Auto-flipping is the wrong default** for the same reason: a large flush
   can displace on the order of a tenth of the served marks, and churning that under
   someone who is reading is worse than telling them. Prefer "N new items — refresh".
3. *Cache validity binds to the view key.* A tile carrying a view key older than the
   last signalled one is renderable but **stale-marked**, and no number-channel value may
   be displayed against it. One equality comparison, and it is what closes §4's
   "pan answered entirely from held tiles" corollary with a mechanism rather than a
   remark.

### 6.2 What the view key is made of, and what each part invalidates

Treating the view key as one monolithic key is over-coarse, and an earlier draft did
*(owner, 2026-08-01)*. Its components invalidate genuinely different things, and folding
them together forces a full re-render for a change whose delta is tiny.

| Tier | Advances on | What it voids for a client |
|---|---|---|
| **Identity generation** | key rotation, idset advance | **everything** — every held `tessera_id` becomes meaningless and row order changes with it |
| **Content version** | flush; accepted deny | what is visible — a small delta (below) |
| **Pin / segment-set version** | compaction | **nothing** |

**The pin is invisible to a client, and the draft was wrong to fold it in.** A client
holds identities, coordinates and scalars; it never sees a row ID. Compaction rewrites
row IDs and nothing else, so nothing the client holds goes stale. A drained pin still
returns `410` on an in-flight *pinned* request — but that is request continuity, not
cache validity, and I10 and I11 are what make the distinction real rather than
convenient.

**Content version is the tier that matters, and ingest dominates it, not denial**
*(owner, 2026-08-01: continuous streams of 10²–10⁶ items/hour, or batches of 10²–10⁷ a
few times a day; denies are rare)*. Two quantities follow, and they differ by orders of
magnitude:

*A deny* moves θ by a relative 1/`V_total` — at 10⁶ visible, on the order of two marks
displaced across an entire viewport — plus the denied entity itself. Genuinely O(1).

*A flush* is the common case and the larger one. Because `served(T)` is a
`tessera_id`-order prefix and new arrivals carry uniformly distributed identities, a
flush adding fraction *f* of the corpus displaces roughly fraction *f* of each tile's
served set. At *f* = 0.1% (a 10⁶/hour stream against 10⁹) that is nothing; at *f* = 10%
(a 10⁷ batch against 10⁸) it is a tenth of the map. **Crucially, the cadence is the
flush, not the arrival**: design §7.2 r24's rule that an entity with no row contributes
to no count means arrivals are invisible until flush, so a 280/s stream advances the
content version once per flush rather than 280 times a second. The batching this
document's §6.1 wanted is already in the ingest architecture; only denies publish
immediately, and that is a security requirement.

The design-level consequence of the *f* = 10% row — that §7.2's acceptance of θ movement
was priced against rare overlay swaps rather than the dominant case — is raised as an
annotation at design §7.2 rather than settled here. It is a question of parameters and
flush cadence, not of correctness: nesting across *zoom* is untouched, and what moves is
stability across *time*, which §7.2 never claimed.

**And this is what §5.1's cursor is actually for.** The draft motivated it on denies,
which are rare and O(1) — the weakest possible case. Ingest is the common case and the
larger delta, and even at *f* = 10% naming the delta beats refetching the viewport
tenfold. The cheap correct move on a content bump is to **refresh the number channel and
keep the mark channel stale-marked**: counts are `range_cardinality` over the mask with
no data file touched (§2.6 step 6), while the gather is the expensive half — so
refreshing numbers eagerly and marks lazily is the honest ordering, not a shortcut. Full
re-render belongs to identity-generation bumps alone.

## 7. Deployment topologies, and the two anti-patterns

Organised on P4's two axes.

| | Who authorises | Token custody | Can fabricate auth data? |
|---|---|---|---|
| **T1 browser-direct** | End user's browser, IdP-issued signed assertion | Browser | No — assertion is verified in-plugin |
| **T2 server-mediated** | Integrator's app server, per end user | Their server, or handed to their browser | **Depends — this is the axis** |
| **T3 notebook** | Python client, analyst's credential | Kernel; crosses to JS for the widget | As T1/T2 |
| **T4 adapter-hosted** | Caller, unchanged | Passed through the adapter | No |

**T2 with verified assertions is the documented default for integrations.** Credential
construction belongs at the integrator's app server because that is where the authority
is; verified assertions (JWS/SAML, trust anchors in the auth plugin, host-enforced
`not_after`) mean their backend may *submit* authority but cannot *mint* it.

**T2(c) — attenuated capability, for multi-tenant integrations** *(third review,
2026-08-01)*. Attenuable capability tokens do not remove minting: the root keyholder can
still mint anything, which is the position the identity provider already occupies. What
they remove is precisely the **middle** component that can fabricate. An integrator
holds a token whose authority is already scoped to its tenant, and may append blocks
that only *narrow* — per user, per session, per expiry — because each block is signed by
a key chained from the previous one while the verifier holds only the root public key. A
compromised integrator backend then fabricates at most **its own tenant's** `M_auth`.
That converts §7's global fail-open into a per-tenant one, which is a quantified
blast-radius reduction verified assertions alone cannot give — under JWS the integrator
either forwards per-user assertions faithfully or terminates the flow itself and becomes
the minting proxy anyway.

It composes as **one more `auth_data` type, not an architecture change**: §6.1 already
declines to require bare claims and anchors trust inside the plugin; verification is
pure computation, so the determinism obligation holds; expiry surfaces as `not_after`
for host enforcement exactly as today; and the effective term set is the intersection
across blocks, which feeds §2.3's content-addressed mask key unchanged — two different
attenuations of one authority resolving to the same terms share one mask, which that key
already handles.

**Biscuit** is the mechanism to evaluate (Eclipse-incubating, `biscuit-auth` 6.0.0,
production use at Clever Cloud and Outscale; its Datalog has set `contains`/`intersection`
and block-origin rules that prevent escalation). **Macaroons are the cautionary tale
rather than the candidate**: HMAC chaining means every verifier holds the minting
secret, which is why the one large modern deployment ended up building a centralised
verification service. The costs are honest and mostly organisational — root-issuance
discipline, since handing an integrator a broad root rebuilds the proxy with extra
steps; a revocation denylist, which is the same class of machinery as the stamp ledger;
and Datalog debugging opacity.

**Two anti-patterns, named as loudly as the three retirement rules.**

*The pooled service token.* An integrator authorises once with a broad credential and
filters per user in their proxy. That is post-filtering rebuilt: every count, density
and label their users see derives from the service token's mask. It passes every
functional test, is invisible in a screenshot, and breaks I2 for every user of that
deployment. It is also the path of least resistance for anyone used to connection
pooling.

*The claim-minting proxy.* An integrator issues genuinely per-user tokens from **bare
claims** — passing P4's letter while remaining one component that can fabricate any
user's authority (§6.1: *"under bare claims, whoever can call authorise can claim
anything"*). The pooled token in per-user clothing, and the reason the taxonomy needs
its second axis.

**The notebook fork.** In a widget, who is on the data path? Direct browser→Tessera is
fast but needs the token in the browser, CORS, and direct reachability that remote
JupyterHub often lacks. Through the kernel comm channel solves both but puts a
blocking single-threaded Python kernel on the interactive pan path. **A third arm
dissolves most of it:** `jupyter-server-proxy`, where the widget's JS reaches a
server-side proxy over Jupyter's own authenticated HTTP path — reachability solved,
credential custody solved, and the kernel stays off the pan path because the proxy is
not the kernel. Prior art for the embed itself is **lonboard** (deck.gl via anywidget);
its whole-dataset residency is precisely our delta.

## 8. Integrations

Four seams worked end to end, chosen as one per integration currency. The exercise was
run *before* the architecture was written, on the owner's steer, and it produced two
findings no top-down pass did (§8.5).

**Owner decision, 2026-08-01: neither Mosaic nor deepscatter is built.** Both analyses
are kept in full, because their value was never the adapter — the exercise is what
produced the mark/number split, the tile-addressed alias, and the independent
confirmation that a mature tile consumer demands the same elision §5 arrived at. What
ships is **deck.gl (ours) and the MVT adapter (theirs)**; §8.4 and §8.5 are retained as
*analysis*, and their verdicts below describe what would be involved were the decision
revisited, not a plan.

### 8.1 The partition that comes first

**Integrations divide by whether the consumer can hold the visible set.**

*Resident* integrations — Mosaic, DuckDB, Embedding Atlas, pandas — are bounded by the
**principal's visible-set size**, not by corpus size, and give exact local aggregates
over `M_auth`. They are notebook-scale by construction.

*Streaming* integrations — deck.gl, MVT, our own core — are bounded by **screen area**
and scale to any corpus and any principal.

The design's central cost claim, *cost scales with screen area rather than corpus
size*, **holds only for the second class.** The export threshold (§8.4) is not an
arbitrary limit but the boundary between the classes: above it the honest answer is
"use a streaming integration", never a truncated table.

**A second axis: does the consumer aggregate internally?** *(survey, 2026-08-01)* A
library survey covering grids, chart engines, scatter renderers and analytics viewers
classified every entry against these two axes without strain, so they are worth stating
as the classifier rather than re-deriving per tool.

*Self-aggregating* consumers — Perspective, any data grid's client-side grouping, chart
engines that bin — compute their own totals from whatever rows they are given. Fed the
**mark channel**, which is a per-tile sample, they present sample aggregates as answers:
the Embedding Atlas KDE failure in table form, and a P2 violation arriving through a
component rather than through our code. Fed the **number channel** — the breakdown
long-form `(value, count)` — they pivot and re-sum correctly, because sums of counts
compose. Means need weighted columns.

**So: a self-aggregating consumer takes the number channel or a full-visible extract,
never the mark channel.** One rule, stated once, covering every such component we might
embed. **Perspective** (FINOS, Apache-2.0; Arrow-native, incremental `table.update`, so
view-key refresh maps directly onto it) is the strongest candidate for mode 2's table and
panel half under exactly that rule, and is the natural counterpart to deck.gl's map half.

**The partition is per-principal, not per-app, and that is a support surprise unless
stated** *(third review, 2026-08-01)*. The visible-set size that selects the class is a
runtime property of each *principal*, so an application built resident works for every
analyst and then meets the export refusal the day a broad-clearance principal signs in.
A resident-class application must therefore either branch on the visible count — cheap
to obtain, one `zoom = 0` full-extent call, at the cost of a dual implementation — or
declare a supported-clearance ceiling. The export refusal should carry the visible count
and the pointer to the streaming class in its detail, so the failure teaches the fix
rather than reading as a limit.

### 8.2 deck.gl — the control case *(verdict: achievable now)*

Seam: `TileLayer.getTileData` plus binary-attribute sublayers. Demands **zero new
server surface** — `meta` and `viewport` suffice today; labels join at Phase 3.

Four findings worth carrying:

*The composition viz §9 flagged is documented, not merely plausible.* Current deck.gl
docs specify non-geospatial tiling: x and y increment from the world origin, each
tile's size matches `tileSize`, `bbox` arrives as `{left, top, right, bottom}`. The
risk narrows from "does this exist" — which viz §9 said would reopen the Embedding
Atlas decision — to "does the arithmetic line up": y-axis orientation, the zoom→z
mapping, and refinement under real sublayers. **Downgrade viz §9's framing further**
*(survey, 2026-08-01)*: the single-cell imaging stack — Viv and Vitessce — runs deck.gl
`OrthographicView` over multiscale tiled pyramids in production, daily, at gigapixel
scale. Non-geographic tiled deck.gl is not exotic; a whole scientific field ships it. The
spike survives because Viv uses its own multiscale layer rather than `TileLayer`, so what
remains unverified is our arithmetic against *that* layer, not the composition itself. **The smallest discharging spike contains
no Tessera at all**: ~50 lines of orthographic view plus tile layer with a synthetic
`getTileData` drawing each tile's index and bbox, asserting index arithmetic at z 0–16,
y direction, abort-on-fast-pan, and cache behaviour. Half a day. **Add one item**
*(third review, 2026-08-01)*: **non-square extents.** §2.5 quantises each axis
independently onto 2¹⁶, so a tile is square in cell space and rectangular in data space,
while `TileLayer` takes a scalar `tileSize`. If it cannot express anisotropic tiles the
fix — pre-scaling y into an aspect-corrected world space — is easy, and it belongs in
the spike rather than in production debugging.

*Our sampler is what makes deck.gl's default refinement look right.* The default
`refinementStrategy: 'best-available'` shows a parent while children load; because
§7.2's nesting makes every child a superset of its parent's marks, that transition is
add-only. The rejected pre-r22 rank-position sampler would have popped on every
refinement — the nesting property earns its keep twice.

*Its tile cache is session-scoped by construction* — per layer instance, keyed by tile
index, default capacity 5× the viewport's tiles — so deepscatter's cross-viewer fault
line does not arise. Its cancellation contract is explicitly fail-closed: on abort,
throw or return falsy so nothing is cached, and never return incomplete data.

*Picking has no u64 problem.* It returns a positional index; identity is resolved
app-side from the Arrow column, so `tessera_id` never enters the render path — the
opposite of MVT. One caught detail: `TileLayer` overrides sublayers'
`highlightedObjectIndex`, so selection highlight must be its own overlay layer.

**deck.gl does not consume Arrow** *(checked against current upstream docs,
2026-08-01)*. Layers take typed arrays or luma.gl `Buffer`s through `data.attributes`;
Arrow ingestion is an explicit *roadmap* item on the `GPUTable` class, not shipped. So
the Arrow path is ours to build — decode with `apache-arrow` JS or loaders.gl's
`@loaders.gl/arrow` (which does handle IPC streams in batches), then hand the underlying
typed-array views to `data.attributes`. That is genuinely zero-copy, since Arrow buffers
*are* views over an `ArrayBuffer`, but **only where the layout already matches the
attribute**. The one library consuming Arrow directly is `@geoarrow/deck.gl-layers`,
third-party and outside core, and it is what lonboard uses.

Two frictions to record rather than discover. The x/y **interleave**: `getPosition`
wants interleaved pairs and we ship separate `x`/`y` columns, so the core does an
O(served) pass — noise at small *k*, an ~80 MB shuffle per refresh at 10⁷ marks. An
Arrow `fixed_size_list<f32,2>` position column would be zero-copy, and is GeoArrow's
point encoding; a candidate **additive** wire change to decide on P2's numbers. Note that
attribute descriptors accept `offset` and `stride`, so several attributes may read from
one interleaved buffer — which means deck.gl consumes interleaved layouts *natively*
rather than merely tolerating them, strengthening that open question. It does **not**
rescue the current layout: stride reads separate attributes *out of* an interleaved
buffer, whereas our problem is the reverse — two contiguous columns that must be fused
into one attribute, which no stride arrangement achieves. And
**cross-tile draw order** for the underlay: tiles render in arbitrary order, so one
tile's opaque cells can overdraw an adjacent tile's marks — mitigable with a
translucent underlay and depth test off, or by separating the layers.

*Stranger vs us.* A stranger gets a working, correctly-masked map in a few hundred
lines and a few days with no Tessera code. What they silently lack is request
coalescing, the multi-stream framing, cross-channel view-key consistency, `{shown, total}`
discipline, the *k* obligation, three-state rendering and re-authorisation. **So the TS
core is load-bearing for conformance and consistency, and merely convenient for
everything else** — which argues for shipping the obligations list and conformance kit
as first-class artifacts in their own right, not as documentation of the core.

**Annotated 2026-08-01 — the paragraph above is wrong at scale, and the correction is
structural rather than a caveat** *(measured by building it: the MVP viewer, `TileLayer` over
`getTileData`, against the 10⁹ fixture)*. Request coalescing is listed as something a stranger
*"silently lacks"* — a quality they forgo. At 10⁹ they do not forgo it, they fall over: **12 of 23
viewport requests were shed with `429 backpressure` from a single browser tab with one user.** A
depth-0 tile over a 5.2 × 10⁸-item visible set takes ~1.3 s, `TileLayer` issues six concurrently
by default, and the compute-admission gate does what it is built to do. That is not degradation;
it is one client denying itself service.

**The cause is a seam mismatch this section did not notice, because it reads `TileLayer` as a
convenience rather than as a request multiplier.** `POST /v1/viewport` is **viewport**-addressed:
one call takes a bbox spanning many tiles and returns every tile's counts plus a flat points batch
that `served` exists to let a reader split. `TileLayer` is **tile**-addressed: one fetch per tile.
Adapting the former to the latter multiplies the request count by the tile count and re-pays the
per-request cost — including, at shallow depths, the expensive part — once per tile. The wire
format already anticipates the correct shape; the adapter throws it away.

Three corrections follow, in descending order of how much they change:

1. **Coalescing is an availability-correctness property, not a nicety.** §8.6's tile-addressed GET
   alias makes a stranger's `getTileData` a five-line function — and this measurement says *the
   alias is the shape that fails*, so shipping it without a coalescing story hands strangers the
   self-DoS as the documented path. Either the alias carries a scale caveat, or the recommended
   path becomes one request per viewport with client-side splitting by `served`.
2. **The claim itself should be scoped**: a stranger gets a working map *at notebook and
   mid-corpus scale*. §8.1's resident/streaming partition has a sibling nobody drew — a
   **naive/coalescing** partition on the same axis, crossed where a shallow tile's cost exceeds
   the admission timeout.
3. **`Retry-After` is on the wire and nothing reads it.** The server sends `Retry-After: 1` with
   every 429; the MVP client does not retry, so a shed tile is lost until the next pan. A naive
   client treating 429 as fatal renders holes; one retrying without backoff amplifies the
   saturation. Neither is obvious, and P6 says a correctness-affecting obligation a naive client
   gets wrong is a defect to design out — so retry policy belongs in the replica store, not in the
   obligations list.

### 8.3 XYZ/MVT — the URL-template seam *(verdict: achievable now; the exemplar)*

One adapter opens MapLibre, OpenLayers and QGIS. Server-side, inside the trust
boundary, carrying the caller's token; `{z}/{x}/{y}` maps to a viewport call over the
tile's bbox, emitting a points layer, a cells layer (underlay counts as polygons, which
is the heatmap affordance via data-driven styling) and later a labels layer whose
anchor/rank properties MapLibre's symbol collision consumes natively — the
maps-industry split (§9) working out of the box. For geographic data, Mercator
quantisation at ingest makes a slippy tile *identically* a Morton prefix.

Two traps. **Feature ids are JS numbers in practice**, so `tessera_id` rides as a
string property with per-tile ordinals for feature-state. And **Leaflet raster cannot
carry an `Authorization` header** — `<img>`-based — while contracts §1 forbids tokens
in URLs, so it works only behind a cookie session or same-origin proxy; that belongs in
the Profile B documentation, since that profile names Leaflet.

**On CDNs, the honest sentence: per-viewer masked tiles are intrinsically CDN-hostile;
the warm stateful tier is the product.** Default CDN keying is by URL, so a shared CDN
serves one user's masked tile to another — the failure viz §7 catalogues for three
geospatial tile servers, relocated one layer out. Posture: `Cache-Control: private`,
view-key-scoped URL segments using a session nonce and never the bearer token, and a
max-age inside the deny-visibility budget. **An adapter cache key carries mask identity
*and* overlay version** — mask identity alone is insufficient, because the mask fragment
is pre-composition and suppressions live in the overlay; §8.5's table shows the pattern
with the servable-label row.

### 8.4 Mosaic — refused at the seam, reached by extract *(verdict: connector refused; extract-hybrid after the export verb)*

The connector seam is one method taking a **DuckDB-dialect SQL string**, including
`CREATE TEMP TABLE … AS SELECT` DDL — which is how Mosaic builds the pre-aggregation
cubes that make it fast, so not an optimisation one can decline.

**This is the roadmap-relevant fact: the deferred SQL surface does not unlock Mosaic.**
That surface was resolved as *a grammar we define over the filter contract*; Mosaic
emits *its* SQL, in someone else's dialect, with DDL. A connector that pattern-matches
and compiles it is a de-facto query planner over exactly the surface Appendix H
refuses, and it breaks on every upstream release. **Refused — and the refusal now cites
the seam's actual shape rather than principle alone.**

What works is **extract-hybrid**: materialise the session's visible set into DuckDB
(WASM in-browser, or native in the kernel) and point Mosaic's stock connector at that.
Aggregates are then exact over `M_auth` rather than sample estimates, and the security
posture is clean — the client holds only authorised rows, and local computation over
them is safe by §9's test. Panning and zooming do not churn anything: the extract is
per view key, re-taken on the change signal, and all interaction is local.

**Its limit is the §8.1 partition and there is no fix**, because the fix would be
Mosaic not being Mosaic. A principal who sees 10⁹ items cannot extract. A
viewport-following working set — the obvious middle path — would make Mosaic's
aggregates exact over *the working set* while presenting them as totals, which is P2's
violation delivered by a system whose entire value is exact cross-filtered counts. **We
refuse the middle path outright**, and the export verb's size guard is where that
refusal is enforced.

### 8.5 deepscatter — the contract that forces our own mechanism *(verdict: build as validation, not as a supported integration)*

Its tiles are **additive**: a point appears in exactly one tile along its zoom path.
Ours are nested prefixes, so a naive adapter double-draws. The correct adapter emits
**delta tiles** — the identity band between the parent's cut and the child's — which is
to say *the deepscatter contract independently demands exactly §5's elision, computed
server-side*. That is the strongest external validation §5 has, and building the
adapter is building the delta machinery.

The "static and shared" assumption proves **not** fatal: its loader fetches each tile
once and caches per session, so per-session-varying contents are tolerated provided
they are stable *within* a session, which our determinism gives. What is genuinely lost
is reconciliation — a suppressed item persists until full reload — bounded by
view-key-scoping tile URLs so a reload cannot mix view keys, and accepted under §4's staleness
ruling. Its `ix` identifier may not tolerate BigInt; the fallback is adapter-minted
per-session dense `u32` ordinals with a server-side mapping, which is legitimate because
the adapter is inside our boundary.

Recommended as a **validation exercise for §5** rather than a supported integration —
its CC-BY-NC-SA licence gates commercial use regardless — with the delta mechanism then
reused in our own core, where it is unencumbered. Note also that its coarse tiles are
filled in **arrival order with no per-point priority**: the field agrees with the
mechanism (a fixed global order determines coarse-zoom membership) and has no
*defensible* order, because nobody else needed one.

### 8.6 What four seams demand in common

1. **The view key** as a client-visible unit of cache validity — all four, independently.
   Promoted to architecture in §6.
2. **A `tessera_id` representation rule**, because u64 is a JS-ecosystem liability with
   three distinct answers: BigInt on Arrow paths; a decimal string in JSON; and
   presentation-local ordinals minted inside the trust boundary where a seam's identity
   slot is too narrow (MVT feature ids, deepscatter's `ix`). Binary-attribute renderers
   need none of it — identity never enters their render path.
3. **A tile-addressed GET alias of the viewport verb.** Three of four seams address data
   *by tile*; our verb is viewport-addressed. An alias with the view key in the path and a
   points-only single Arrow stream — no multi-stream framing — makes a stranger's
   `getTileData` a five-line function, removes their framing parse, and gives browser
   HTTP caching a correct URL shape for free. Identical selection underneath, so it
   costs nothing server-side. This is the clearest thing the integrations-first method
   surfaced.
4. **Bulk export** as load-bearing architecture, not a gap-list item — the only route to
   the resident class, and the SQL surface is not a substitute for it. **This survives
   the 2026-08-01 decision to drop Mosaic**: export's day-one consumer is the notebook
   itself — "give me this selection as a DataFrame" is mode 1's first request — and
   Mosaic was only ever the most demanding thing reachable through it.
5. **Count-blindness**, which is what §2 is built on.

**Annotated 2026-08-01 — two additive contract changes, owner-approved, that move the hardest
integrator obligations to the server side** *(from building the client and measuring it; the
byte-level statement belongs in the contracts spec, which governs)*.

The exercise that produced this section asked what four seams demand. Having now *been* the
integrator, two further demands are clear, and both follow from **P6** — the naive path must be
correct, so an obligation a naive client gets wrong is a defect to design out rather than a line in
the obligations list.

**(1) The request should carry a mark budget, not a zoom.** `{slice, bbox, budget, k}` with the
server choosing the depth, alongside the existing `zoom` form.

*Why it is the highest-leverage change available.* Depth choice is the single hardest thing the
client does and the least obvious: §7.2 fixes marks **per tile**, so marks **on screen** is
`m_target × tiles-in-view`, and at depth 0 a viewport holds one tile — which is why the
tile-addressed MVP drew 17 marks at full extent. Getting it right took a measurement campaign
(`probes/2026-08-02-viewport-and-underlay/`), produced a formula that needs a `min(·, V_total)`
saturation term to avoid being wrong by three orders of magnitude for sparse principals, and needs
a one-directional calibration loop to avoid serving a subset of what is already drawn. **No
integrator will derive that from an OpenAPI description.** The server already holds every input:
tile arithmetic, `m_target`, and `V_total`.

*Why it looks safe, stated as a starting point for the register pass rather than as a conclusion.*
The server choosing what to serve is entirely within its remit — this removes a client decision
rather than adding a client capability. Nesting is untouched. `served` stays a pure function of
(mask, corpus, k, viewport) with `budget` joining the tuple, so §11's determinism survives. And the
chosen depth is derivable from quantities the client can already request, which is C18's argument.
**It still needs its own I2 pass and possibly an Appendix C entry before it is built.**

**(2) Length-prefix every stream in the frame, not only the first.** Today only the tile stream
carries a `u32` prefix; a reader that wants the points or sub-cell streams must walk Arrow's
encapsulated messages to find the boundary. `tessera-wire`'s `payload` doc argues that relaxation
deliberately, and the argument holds for *today's* readers — but it is a per-language porting cost
for every non-JS consumer, and it is where this client's one genuinely subtle bug lived: Arrow
folds padding into the metadata and body lengths it reports, so the obvious alignment arithmetic
over-advances by four bytes and desynchronises on the second message. Prefixing all three is
additive, costs 8 bytes, and deletes that class of failure from every port.

**Both are additive and neither breaks an existing caller.** Neither is built; both want the
contracts spec's treatment first.

## 9. The primitive inventory

The test, corrected after an owner ruling on 2026-07-31, is **not** "is this an
aggregate?" but **"can this be computed from data the principal already holds or is
authorised to see?"** If yes, it is safely client-side however aggregate-shaped it
looks, and the only remaining question is whether serving it is a convenience worth the
surface. A quantity that differs per principal is not thereby a leak: I2 objects to
quantities derived from data the principal *cannot* see.

**The decision rule that falls out, and which governs the whole inventory:** *if it
renders as a number, it is served exact; if it renders as a mapping — colour, size,
transfer function — the client may compute it.* The owner's scale ruling is the mapping
half; the viz architecture's settled "4,120 items selected (47 shown)" is the number
half.

**Encodings are configuration first, computation second.** Scale domains, palettes and
size ramps may be (a) **deployment or client configuration** — viewer-independent,
stable by construction, disclosing nothing, with the exact precedent of contracts §3.2
publishing the `selection` constants on the argument that *"they disclose nothing: all
four are deployment constants, identical for every principal"*; (b) **client-computed
from held data** — safe, free, imprecise, and it moves as you pan; or (c)
**server-computed over the visible set** — masked, pan-stable, costs a query, and is
the existing region-breakdown shape widened. (a) is the recommended default, and it
serves mode 2 for free because a buildable UI needs encodings declarative anyway. The
one trap is narrow: **never serve an unmasked domain** — structurally the same mistake
as §7.7's rule that extractive-tier background frequencies come from a fixed public
reference corpus rather than the live one.

Applying the test across the inventory, almost everything encoding-shaped passes.
**Three findings survive:**

**Offered vocabularies fail where derived ones pass.** A domain the client derives is
safe. An enumeration Tessera *offers* as an affordance — a filter picker, autocomplete,
a served list of available categories — can name values existing only in invisible
items. This is C11's exact shape, and the finding is that **C11 generalises**: state
the rule once for all offered enumerations rather than rediscovering it per widget.

**The change signal fails the test**, and is handled in §6.

**Cross-principal style sharing is a residue of the secrecy/truthfulness division.** A
client-computed domain is safe for its own principal; an application that shares one
principal's derived style with another ("use my colour scale") transmits information
about the first's visible distribution — C17's out-of-band family, created *above*
Tessera. Closed by one rule in the toolkit: view-derived encodings are session-scoped
artifacts; shared styles go through caller metadata.

**Two expectations the embedding-viewer lineage has already set, which this document
should answer rather than let users discover** *(survey, 2026-08-01)*. TensorBoard
Projector, WizMap and Latent Scope taught this audience what a data map does, and two of
their signature interactions are unaddressed here.

*Click a point, see its nearest neighbours.* **Available now, in the plane, without the
vector store** *(owner, 2026-08-01, revising the 2026-07-31 scoping which had excluded
the plane option; the earlier ruling stands for what drill-down means, but no longer
excludes this)*.

Nearest neighbours in projected space is a **bounded read over nearby Morton cells**,
because a point's spatial neighbours are confined to a small number of cells: expand a
ring around the query coordinate, intersect with the mask, gather the survivors, sort by
true Euclidean distance, take *N*.

Three properties make it cheap and clean rather than a special case.

It is **§8.2-compliant by construction**, not by argument. That section requires ranked
filters to be expressed as thresholds with top-*k* applied *after* intersection, and C10
records "candidate push-down" as the mitigation that closes it. Here the Morton ring
**is** the candidate push-down, the mask intersects before any ranking, and the ordering
runs last over survivors — so the principal receives the nearest among items they may
see, which is the correct semantics, and no post-filtering step exists to leak through.

**Sizing costs nothing before the gather.** `range_cardinality` returns exact masked
counts over a contiguous range with no data file touched (§2.6 step 6), so the server
walks up depths until the containing cell holds at least *N* visible entirely in bitmap
arithmetic, then performs one bounded gather over at most nine contiguous ranges. The
standard grid termination condition applies — expand until the *N*-th distance found is
no greater than the distance to the search-region boundary — or a point just outside the
block can beat one in a far corner.

**It adds no channel.** The radius needed to find *N* visible neighbours is a function of
local masked density, and §7.1 already returns that exactly for any tile at any zoom. The
quantity is derivable from what the principal can already request, which is C18's
argument unchanged.

Two distinctions to keep sharp. **This is not the deferred sort demand of §15**, despite
involving an ordering: that one is an O(visible) gather-and-sort over `M_sel`, this is a
bounded top-*N* over a candidate set small by construction — different cost class,
different verdict. And **this is not semantic similarity**: §8.3's trap applies at full
strength — *"the 2D coordinates are a projection; similarity in the source space is not
similarity in the plane"* — so the affordance must be named **nearby on the map** rather
than "similar documents", with the Phase 4 vector operand remaining the answer for
source-space similarity. Naming it honestly is what stops a viewer reading projection
artefacts as semantic claims.

**Depth is a free parameter, independent of the view's zoom, and it defaults deep**
*(owner, 2026-08-01)*. The point of this interaction is *"what is near this point that
isn't yet rendered"* — so tying the read to the viewport's depth would return the marks
already on screen and tell the user nothing. **This is drill-down in §7.4's sense**, the
tile behind a mark generalised to the neighbourhood around a point, and its value lies
exactly in what the sampler declined to draw.

Depth remains *available* as a parameter, because the shallow end is the right behaviour
for a different interaction — hover highlight, live lasso feedback — where the answer
should be marks the viewer can see. And because priority prefixes nest, the parameter
**refines monotonically**: deeper adds neighbours and never removes them, so a client can
expand progressively without anything popping out. That is §7.2's nesting property doing
work in a second place.

**The invariant argument is unchanged, and derivability is trivial.** A priority prefix
over masked candidates is §7.2 verbatim, so I7 holds by the same reasoning. And a
deep-depth neighbour query returns a **subset of what a zoomed-in viewport request over
the same region already returns** — a client could zoom, fetch and sort by distance
itself — so under P3 this is a convenience over the existing verbs rather than a new
capability, admitted on that basis with no register entry.

**One consequence to design for rather than discover: undrawn results must not be drawn
into the mark layer.** The client now holds identities that the sampler deliberately did
not serve for the current view; painting them onto the map would locally corrupt
mark-count-as-density, which is §7.3's entire premise — a neighbourhood the user has
expanded would read as denser than its surroundings for a presentational reason. They
belong in a panel, or in a visually distinct overlay a viewer reads as *expanded* rather
than as data. And P2 applies as usual: report *N* shown against the masked total within
the radius, never a bare *N*.

Shape: a mode of the region verb parameterised by a query coordinate, *N*, and a depth,
rather than a sixth verb — the adaptive expansion is server-side, and the result is the
region machinery with a bounded ordering on top.

*Lasso, tag, iterate.* Interactive annotation is the loop those tools are built around,
and it is a **write** path the viewer verbs deliberately do not carry. Name it as an
app-layer pattern — annotations keyed by `external_id` and stored on the integrator's
side, joined at display time — rather than leaving it to be discovered as a missing
feature. It is the most common uncovered expectation in the survey.

*(WizMap's overview — density contours plus labels — is the underlay plus gated labels
we already serve. Confirmation, nothing to add.)*

**The one primitive the design is materially short of** is label placement input. The
maps-industry pattern — Google- and Apple-style pipelines precompute per-feature
**anchor, importance rank and zoom range** server-side and let the client do collision;
stability comes from stable anchors and ranks, not clever collision. Phase 3's labels
verb carries `(node_handle, label, tier)` and none of those. Client-derived anchors
would jitter per viewport: wrong rather than leaky, but wrong.

Three smaller reshapes. **The density underlay should be the expected default**, not an
optional garnish: every system in the field at ≥10⁷ renders density as a field or
aggregate cells (datashader rasterises; CARTO ships quadbin tilesets, which is a Morton
quadkey independently reinvented; Foursquare ships H3), and mark-count-as-density is our
novel part — the field's history says it should not carry the load alone. Adopt
**histogram-equalisation** (datashader's `eq_hist`) as the recommended count→colour
transfer, computed client-side from served counts; it is their hard-won answer to
multi-decade distributions and beats a fixed log transfer.

**Annotated 2026-08-01 — "expected default" is right and unreachable at the resolution the
mechanism offers** *(MVP viewer; owner observation: "waaaay too low resolution —
ideally we want it full res from day one")*. The argument above says mark-count-as-density must
not carry the load alone. It cannot be relieved by an underlay that is *blockier than the thing it
is relieving*: at `serve.max_underlay_offset = 4` a 512-px tile carries 16 × 16 sub-cells — **32-px
blocks** — which reads as a mosaic rather than as a density field, while the marks it sits beneath
are individually placed. Full resolution means offset 9 (512 × 512 = 262,144 sub-cells per tile).

**The binding limit is not the offset but `serve.max_underlay_cells`, whose default is 8,192 —
for the whole request.** That is thirty-two tiles' worth at the current offset and *one
thirty-second of a single tile* at full resolution, so the guard would have to move by four to
five orders of magnitude. Naming it matters because it is the knob that actually refuses, and
because a guard sized in total cells is the right shape for a sparse pair encoding and the wrong
shape for a raster one — the encoding decision below changes what the guard should even count.

Two things break before full resolution is reachable:

- **The evaluation is per sub-cell.** The engine issues one `count_range` per cell and evaluates
  `4^offset` of them per tile (`underlay_cells_evaluated` exists to measure exactly this). Measured
  cost at offset 3 was ~0.12 ms per tile; scaled naively to offset 9 that is ~0.5 s per tile, which
  is not serveable. The alternative — one pass over the mask's set bits in the tile's range,
  bucketed by Morton prefix — is O(cardinality) rather than O(cells), so it wins where the tile is
  dense and loses badly at low zoom where the visible set is 5 × 10⁸. **Neither dominates**, which
  makes this a route-chooser question of the same shape as the selection-route chooser this
  repository already carries, and it wants measurement before a route is picked.
- **The wire encoding is sparse.** `(cell u64, count u64)` is 16 B per non-empty cell, chosen when
  cells were few and most were empty. At full resolution the density inverts: a dense raster with
  implied geometry — the offset and the parent prefix already name the grid — is both smaller and
  simpler, and it is what a `BitmapLayer` or a WebGL texture wants anyway. That is an **additive**
  wire change, and it should be decided on the same numbers as the route.

Note the second-order effect on §8.6's alias and on the annotation at §8.2: a full-resolution
underlay is per-*viewport* raster-shaped, not per-tile pair-shaped, which pulls in the same
direction as viewport-addressed requests. The three findings of this date are one workstream.

**Selection should become a
content-addressed filter operand** rather than a one-shot verb, so it composes with
other filters, caches per §8.5, and gets §8.1's matched-versus-visible highlight
affordance for free. And **per-scalar histograms are the one new verb mode 1 needs** —
distributions are the first thing anyone plots — implementable as build-time
bin-membership bitmaps, one `and_cardinality` per bin, no scan.

**Empty, loading and refused are three states and may never be collapsed.** Fail-closed
is a server property a client can destroy in one line by rendering a failure as an
empty region: an empty viewport and a failed viewport are semantic opposites, zero
versus unknown, and collapsing them converts fail-closed into fail-misleading. This is
a conformance item, and it is why developer experience and observability earn
architectural status here rather than being tooling concerns.

**There is a fourth state, created by §4's own staleness ruling and previously unnamed**
*(third review, 2026-08-01)*: **shown-but-stale** — drawn under view key *E* while the
change signal reports *E′ > E*. The ruling makes this legitimate; nothing makes it
*visible*, and an unmarked stale display is the truthfulness failure the staleness
concession quietly buys. The fields that handle this honestly are the regulated ones —
delayed market data must carry a delay badge — and the general pattern is an "as of"
affordance owned by the core and surfaced by default rather than opted into. It belongs
beside the trichotomy as a conformance item, and it is the display half of §6.1's rule
3.

### 9.1 Derived artifacts — moved

Clusters, labels, hulls, contours, aggregation cells, boundary polygons and edges form a
second class: **named subsets of the point set plus an attachment**, whose visibility is
tied to point visibility. The design gates three of them by three different rules (§7.6
containment, §7.5 threshold, C2 none), and the framework connecting them —
together with the cardinality axis that separates artifact-scale members from edges, the
three forms a polygon can take, and a live question about differencing the frontier — is
**`derived-artifact-gating.md`**.

It was split out on 2026-08-01: it had grown to a quarter of this document while answering
a question that is not a client question. It generalises two sections of the specification,
its audience is a security reviewer, and it has already produced annotations at design §7.5.

Three of its conclusions are load-bearing here and are relied on above and below:

- **Contours are served as nothing.** They are a client-side derivation over the underlay's
  masked sub-cell counts, so per-viewer isolines cost no new server surface.
- **Cluster hierarchies ride the verbs, not the tiles**, and should speak supercluster's
  interaction idiom, which maps almost one-to-one onto the Phase 3 frontier.
- **The number channel is really the exact masked-aggregate channel, and geometry travels
  on it.** A client drawing a hull around the *k* points it holds commits §2's
  sample-as-set error in geometry rather than in numbers — which corrects the naming in §2.

## 10. The client stack

**One headless core, in TypeScript**, owning everything invariant-bearing: session and
token lifecycle, viewport-to-range arithmetic, tile scheduling and prefetch, Arrow
decode, the replica state of §3 and §5, filter state, frontier and label selection,
cross-channel view-key consistency (§2), and *k* (§4, P6).

**Its scheduler has a gap the design does not cover, and the point-cloud field has
solved it** *(survey, 2026-08-01)*. §7.2 bounds marks **per tile**; nothing bounds the
total drawn across a viewport, and the drawn-mark budget workstream's 10⁷ ambition is a
*global* number. Potree enforces exactly this — a global `pointBudget` spent across
competing nodes by screen-space-error priority, with eviction — and CesiumJS's
`maxScreenSpaceError` traversal is the principled form of "which tiles at which depth"
that our scheduler would otherwise improvise. Both are deployed prior art for the part
this design has not written: **how a client spends one budget across tiles that all want
it.** Note the boundary carefully — this is a *client-side rendering* budget, and it must
not become a second selection rule: the served set is the server's answer, and a client
that drops marks to fit a budget is choosing what to draw, not what is visible. Which
marks a client declines to *draw* is presentation; which marks it is *served* is I7.

**Annotated 2026-08-01 — measured, and the gap is wider than "spending a budget across
competing tiles"** *(MVP viewer against the 2.4 × 10⁶ and 10⁹ fixtures; owner observation)*. The
paragraph above frames the budget as an *allocation* problem: given the tiles a viewport wants,
how is one budget divided among them. The measurement says the binding decision comes one step
earlier — **which depth to request at all** — and that without it the budget cannot be spent at
low zoom no matter how it is allocated.

Marks drawn across the whole viewport, principal granted every term: **17 at depth 0, 50 at depth
1, 242 at depth 2, 456 at depth 3.** Per tile that is 12–57 throughout, so §7.2 is behaving
exactly as specified and the swing is entirely the tile count. **A viewport at depth 0 contains
one tile**, and one tile's `m(T)` is the entire budget available to it — there is nothing to
allocate. Design §7.2 carries the matching annotation and refuses the obvious alternative
(making `m_target` depend on tiles-on-screen) on its own grounds: θ is viewport-invariant so that
it does not move on a pan, and coupling it to the viewport reintroduces precisely that churn.

So the scheduler's first job is **decoupling requested depth from viewport zoom**. Because
priority prefixes nest, requesting depth *d* under a shallow view is a superset of the natural
tile and pops nothing — the same nesting property §8.2 credits for making `best-available`
refinement look right, doing work in a third place. The arithmetic is `marks ≈ m_target · 4^d`.

**Two constraints this exposes, neither of which the Potree/Cesium prior art carries**, because
their budgets are spent against a static local octree rather than a per-request server:

- **`serve.max_tiles_per_request`, which turns out not to bind — but the relationship should be
  documented.** The arithmetic (design §7.2's annotation of the same date) gives a tile count of
  **`B / m_target`, independent of zoom**: 3,125 tiles for a 5 × 10⁴ budget at `m_target = 16`.
  The guard's default is 262,144, so there is room by two orders of magnitude. What survives is
  that an operator lowering it **silently caps the achievable budget at
  `m_target · max_tiles_per_request`**, and neither knob's documentation says so.
- **Depth choice and request coalescing are the same mechanism.** Asking for depth 6 at zoom 0 is
  only affordable as *one* viewport-addressed request; as 4,096 tile-addressed fetches it is the
  §8.2 self-DoS multiplied. The budget scheduler and the coalescing fix are therefore one
  workstream, not two.

Viz architecture §1 already
fixes this boundary; this document fills in its protocol half.

**The core is a first-class distributable, not a reference implementation.** It is the
recommended path for anything running in a browser, whoever's frontend that is — which
narrows the population that cannot use it to non-JS consumers. §8.2 measured what it is
worth: a stranger gets a working map without it and a *conforming* one only with it or
with a reimplementation of the obligations list.

**Three layers, each usable alone**, because the three usage modes want different
surfaces: a **session client** (plain async verb calls, no state — what a REST user
would write anyway); a **replica store** owning cache, view keys, reconciliation,
invalidation and *k*, exposing observable state projections (Mosaic's `Selection` and
`Param` semantics are the closest studied prior art for this layer and worth reading
directly); and a **drop-in deck.gl layer**, which for the map audience is the single
most familiar artifact we could ship.

One mechanical rule closes the persistence residue of §4: **the session owns the cache
lifetime and drops it on token change**, so cross-principal persistence is impossible by
construction rather than by documentation.

**Python is two unequal layers.** A headless data client — authorise, query, Arrow or
DataFrame out, with `{shown, total}` types — and an optional display layer embedding
the TS core through an anywidget (owner ruling: one core, Python embeds it). **No
invariant-bearing logic is implemented twice**: Python never computes a masked
quantity, never gates a label, never decides what is drawn. Note that the widget pans,
so the replica machinery runs there too — in the core, inside the widget's browser
context, not reimplemented in Python.

**Two documentation artifacts rank as deliverables**, not as documentation of the
core: the **client obligations list** (every rule the server cannot enforce) and the
**conformance kit**. The kit's subject is truthfulness, not secrecy (§4) — displayed
counts sourced from the number channel, both numbers on every selection, *k*
non-decreasing, the four display states, and §2's one-predicate view-key assertion. §11's
determinism is what makes it shippable.

**But the kit cannot *bind* a client we cannot inspect, and an earlier draft claimed it
could** *(third review, 2026-08-01)*. A stranger's frontend consuming the REST surface
exposes no displayed state to assert against; the kit binds our core, and any client
whose display layer a harness can drive. Every field that solved this solved it
**socially**: the Certified Kubernetes model binds through a trademarked mark plus a
self-run suite whose results gate the mark — the suite is the *evidence*, the brand is
the *enforcement*. The correction changes what gets built rather than what gets written:
the kit needs a **driver harness** capable of running against an integrator's own
application through a headless browser with DOM-level assertions, not merely a set of
canned transcripts, and it wants a certification-mark policy beside it — a
"Tessera-conformant" claim that is licensed rather than assumed.

**Mode 1 needs a first-party local mode** — `tessera.open(path)` to a map in a few
lines, with a loudly-marked allow-all plugin and a localhost self-token for the
single-principal case. If we do not ship one, a practitioner will write a permissive
bare-claims plugin, and that artifact will graduate into a deployment as §7's
claim-minting proxy. The ramp property — the same API from laptop to compartmented
deployment — is what made DuckDB and SQLite adoptable, and here it doubles as the safety
story: the easy path teaches the deployment-safe shapes. Its companion is the
**acked-is-not-visible** affordance: an ingest acknowledgement is a durability receipt,
not a visibility promise (§11.2), so a practitioner who ingests, gets identities back,
queries and sees nothing will conclude Tessera is broken unless the SDK surfaces
"N accepted, M visible, flush pending" from the watermark.

## 11. Determinism as a product property

`served` is a pure function of (mask, corpus state, *k*, viewport) — §7.2 contains no
server-side randomness. Three payoffs nobody has claimed: cache-correctness
tests over decoded content; a **record-replay conformance harness** (a scripted server with canned
suppressions mid-session, asserting displayed state) which is how §10's kit becomes
operational against clients we cannot inspect; and cross-session reproducibility as a
documented feature for notebook users — same credentials, same corpus, same picture.

**What is determined is the served set, not its bytes**, and the distinction is worth holding
because it is easy to spend. Two responses that encode the same served set are equally correct;
the service does not promise they are byte-identical. They are today, at any configured thread
count, and design §10.4 records why — but as an implementation detail a future optimisation may
remove, not as a contract. A client or a test that compares response bytes rather than decoded
content is depending on something nobody has offered it.

## 12. Customisability, and the data-shape stretches

**Mode 2's "buildable, not static" requirement is substantially a configuration-schema
problem before it is a plugin problem.** §9 already moves encodings into configuration;
what remains is a three-layer structure: the headless core, a **state-projection
extension API**, and source-distributed components — editable rather than merely
themable. The load-bearing rule: custom layers, panels and interactions consume the
core's *state projections* (tiles, counts, served points, labels, selection) and
register interactions through a **gesture-to-verb mapping**, and **never raw fetch** —
so extensibility cannot route around the verbs and §10's conformance survives plugins.
Composition prior art worth naming: Grafana's panel contributions and VS Code's
contribution points.

**The stretches are new orderings, not new engines**, and Appendix H says so directly:
*"nothing in the machinery is two-dimensional… several orderings can coexist — one
permutation each, one range structure each"*, a consequence of I4.

*1-D (timelines).* A second row space ranked by the temporal quantity; tiles are dyadic
prefixes, where the prefix property is trivial; density is count per interval; §7.2
transfers unchanged except that θ's per-depth factor becomes ×2 rather than ×4. The
actionable consequence is to **generalise the sampler's branching factor**
rather than hard-code 4, which also makes the client's zoom semantics
geometry-independent. Keep this distinct from Appendix F, which is the *filter* form of
time; a timeline view is the *ordering* form. The overview strip is not a new verb at
all — it is §7.1's tile counts over the second ordering.

*Geographic.* A Web Mercator projection into the quantised extent makes an XYZ slippy
tile *identically* a Morton prefix, so the engine already speaks it; CRS handling is an
ingest contract and §8.3 covers serving. One honest caveat rather than a fix:
mark-count-as-density becomes screen-space density, which under Mercator is
latitude-distorted as ground density. A basemap plan is mandatory — the OSM tile
servers' usage policy forbids production load — and self-hosted PMTiles is exactly right
here, because basemaps are public.

What stays refused is already refused: time as a third Morton dimension (§9), and
re-fitted rather than transformed projections (§2.4).

## 13. Sequencing: the mode gradient

The minimum primitive sets differ by mode in a way that *is* the roadmap.

**Mode 3** (stranger's frontend) needs **zero new primitives**. What it needs is an
OpenAPI 3.1 description of the finished surface, the obligations list, and the
conformance kit — the last of which, per §10, is a driver harness plus a certification
mark rather than a transcript set. Its unit of adoption is a documented stable verb, not
a feature, so the cheapest adoption wins here are documentation, not engineering.

**Mode 1** (notebook) needs mode 3's set plus the local mode, Arrow exports carrying
`{shown, total}`, the acked-is-not-visible affordance, the bulk-export verb with its
refusing threshold, and **one** new verb: per-scalar histograms.

**Mode 2** (buildable UI) drives every remaining new primitive: label placement inputs,
underlay-by-default, selection-as-operand, binned counts for linked views, the change
signal, and frontier marks.

Two items sit outside the gradient because they gate rather than deliver: the **half-day
orthographic spike** (§8.2), which discharges viz §9's named assumption or reopens a
settled decision, and belongs in the first slice of implementation whenever that starts;
and the **tile-addressed GET alias** (§8.6), which is cheap and improves three seams at
once.

**Export and SQL are not competing for a slot.** SQL serves the BI family by aggregating
server-side; export serves the resident class by shipping rows. Different populations,
different mechanisms — and per §8.4, SQL does not reach the resident class at all.

## 14. Options considered and rejected

Recorded because each will be proposed again.

**Folding authorise into the first viewport call.** Raised and withdrawn by the owner,
2026-07-31. It would collapse §2.2's two-stage split and break I6, which requires
authorisation to be computed once from an explicit input rather than inferred from a
request.

**A pooled service token; a claim-minting proxy.** §7. The two integration mistakes that
pass every functional test.

**Client-derived membership** in reconciliation. §5. Fail-open on newly-suppressed
items.

**Prepared filter handles.** Ceremony: filter expressions are a few hundred bytes, so the
wire saving is nil. Content-address the expression server-side instead — matching how
masks are already keyed — with a specified canonical form, noting that a canonicalisation
collision is a wrong-answer bug and not a disclosure, because the mask intersects
afterwards. Handles earn their place only if an operand ever carries a large payload; a
Phase 4 vector filter shipping a query embedding on every pan is the trigger.

**Flight SQL as a query planner.** Refused; Flight as a *transport* is fine. §8.4
upgrades this from principle to a concrete seam analysis.

**A viewport-following extract for Mosaic.** §8.4. Not merely awkward — actively
misleading, and the one shape to refuse outright.

**PMTiles and static tile archives for data.** The ecosystem's favourite performance
move, and the §15-rejected build-once-serve-to-everyone approach in current dress.
Correct for basemaps. The positioning that falls out: *Tessera is to per-viewer point
data what a tile server is to public data — what you use precisely when the static
archive is forbidden.*

**View-key-delta naming.** Not rejected — deferred pending a written safety argument. §5.

## 15. Open questions

- **Does `TileLayer` compose with `OrthographicView` at our index arithmetic?** The
  y-axis orientation and zoom-to-depth mapping are undetermined without running it.
  §8.2 specifies the spike.
- **The reconcile table** (§6) needs writing out once the change signal is designed.
- **Token session-binding semantics** under T2(b): §2.3 says tokens are bound to the
  issuing session, which is the app server's, not the browser's. Pin this before
  documenting T2(b) as the default.
- **The export threshold's value**, which §8.1 makes a class boundary rather than an
  arbitrary limit — set with the region breakdown threshold at deployment review.
- **Notebook token custody**: the widget must never serialise a token into saved output.
- **Whether the `fixed_size_list<f32,2>` position column is worth an additive wire
  change** — decided on P2's numbers, not now.
- **Whether position storage should be Morton-derived rather than `x`/`y`** *(owner,
  2026-07-31; three variants after the third review, 2026-08-01)*. §2.6 stores `x`, `y`
  as `float32` "as supplied (quantisation is for codes, not storage)", so `morton.u32`
  duplicates their quantised high bits; what Morton cannot recover is only the residual
  *within* a cell. Every variant below is **8 B/row against 12 B — 4 GB at 10⁹**, the
  same magnitude and the same argument as r5's narrowing, and none is dominant:

  | Variant | Search column | Random touches per served point |
  |---|---|---|
  | today | `morton.u32`, contiguous | 2 (`x`, `y` are separate buffers) |
  | **A — split** (owner) | `morton.u32`, contiguous | 2 (`morton`, `residual`) |
  | **B — fused** `u64` | high half at stride 8 | **1** |
  | **C — fused + sparse index** | index of every *n*-th code | **1** |

  The **gather is neutral between today and A** — Morton is not read per
  served row, so A trades two touches for two — which is what makes B interesting: one
  fused code halves the gather's random touches. B's cost is exactly the argument §2.6
  makes for the `priority` column one level down — *"a cheap prefix must be physically
  contiguous… reading the high 2 bytes of a `uint64` array at stride 8 touches every page
  holding any value"* — so B doubles the search column's footprint and halves its
  values per page. **Which cost dominates depends on *k***: at sparse *k* the scattered
  gather dominates and B wins; in the 10⁷ scan regime the range is read whole and the
  contiguity objection weakens. **C** is the shape that takes both — fuse, and restore
  cheap search with a sparse index binary-searched to a page, then scanned within the
  column the gather reads anyway.
  Three further notes. B and C are §5.2's own stated future — past ~4×10⁹ the grid must
  widen to 64-bit codes, so filling r5's deleted half with signal makes that widening a
  no-op rather than a second format bump — and their compression structure is cleaner
  than compressed floats, since the sorted high half delta-codes to nearly nothing while
  the maximum-entropy low half stays fixed-width for mmap-and-slice. The low bits must
  **not** join the sort key under any variant, or the wire ordering contract and the
  served-prefix machinery are disturbed. And precision is a non-issue: 32-bit fixed point
  over the extent is uniformly *more* faithful than `float32`; what is genuinely lost is
  bit-exact round-trip of supplied floats and extent-independence of stored values, both
  contract changes the oracle inherits. The wire need not change — the server dequantises
  during the gather, and that pass **is** the interleave pass the item above wants.
  **Belongs to the drawn-mark-budget spec as P4 arms**, not to this document; recorded
  here because it was raised during this design.
- **Whether a shared WASM kernel should own the invariant-bearing arithmetic** *(third
  review, 2026-08-01; owner ruling: record as an open question, do not restructure §10)*.
  §10's TypeScript core reimplements Morton and tile arithmetic that `tessera-spatial`
  already owns, plus the nesting and *k* rules and view-key comparison — precisely the code
  where an engine/client disagreement would be silent and conformance-relevant. A small
  crate compiled to WASM would give one implementation. The boundary matters if it is
  ever taken: **arithmetic only**, with the replica store staying TypeScript, because
  chatty stateful APIs across the WASM boundary are where Rust-in-the-browser goes wrong.
- **Is the view key a readable coordinate, or only a cache-busting nonce?** *(third review,
  2026-08-01)*. Every sync engine surveyed answers "readable, with a retention window".
  It matters at §6.1's flip: a pan mid-flip may need one more tile under the *old* view key
  to keep the outgoing snapshot complete. If the server will serve a still-retained view key
  — the same retention shape as pins and `410` — flips never tear; if not, the client
  force-flips early or shows holes. Serving a stale view key briefly keeps an accepted
  suppression visible within that view key's responses, but bounded by the same deny-visibility budget
  as the flip deadline, so it spends §4's existing concession rather than a new one.
  **The reconcile table cannot be written until this is chosen.**
- **Licence review** for any Grafana or Metabase plugin work (both AGPLv3: a plugin is
  standard practice, embedding or forking the host is an AGPL event).
- **Multi-slice comparison** has no client-architecture position yet.
- **Sort, for table-shaped consumers** *(survey, 2026-08-01; owner: record, do not decide)*.
  Enterprise grids match the drill-down cursor almost exactly — AG Grid's server-side row
  model, or TanStack Table's manual mode (MIT; AG Grid's server-side model is
  Enterprise-paid), both wanting `{startRow, endRow, sortModel, filterModel} → {rows,
  totalCount}` — but they expect **sort by any column**, and Tessera has one serving
  order, `(morton, tessera_id)`. Sorting a region result by an arbitrary scalar over
  `M_sel` is an O(visible) gather-and-sort, outside Appendix H's counting-engine boundary
  and in the same class as masked means. Three positions, none taken: **refuse** (server
  order, stated — honest, but integrators will then sort the *page* client-side, which
  silently sorts a sample); **bounded top-N** by walking the build-time value-bin
  bitmaps the histogram verb needs anyway, gathering only until N rows fill — stays near
  the counting boundary and serves "top 50 by score", which is what panels actually want;
  or **full sort behind an explicit cost gate**, most faithful to the grid contract and
  the thing §8.2's "pipeline, not a planner" rule exists to keep off the request path.
  Decide when a table consumer exists; the top-N option's dependency is itself only
  planned.
- **Verify Elasticsearch's `_mvt` under document-level security** before citing it.
  If it holds, it is production evidence that per-viewer vector tiles are viable and are
  served private rather than CDN-fronted — external validation for both §8.3's adapter
  and its CDN posture — and it is the competitor's geo mode, filtering per query where we
  materialise per viewer.
- **Control-plane client story**: build-credential custody in a notebook, given that node
  iteration must never be reachable from a user token (§2.5).

## 16. Provenance

**r2 (2026-08-01) applies two decisions and changes no mechanism.** Decision
[0029](../decisions/0029-view-key.md) names the composite this document is largely about: the
coordinate **(mask, overlay version, slice, *k*, idset)** within which `served(viewport)` is
stable was a fourth thing called "epoch", and is now the **view key** (§6). *Key* rather than
*state* because the viewport is deliberately **not** one of its components — a served viewport is
stable *across* viewports within one view key — and a name that needed a disclaimer in every
document using it was the wrong name. Decision
[0030](../decisions/0030-determinism-is-not-a-guarantee.md) separates §11's two claims: `served`
is a determined function of (mask, corpus state, *k*, viewport), which is promised, from
byte-identical responses, which hold today and are not.

Brainstormed with the owner 2026-07-31. Reviewed by independent agents with no stake in
the plan being right, per CLAUDE.md's working method.

*Reviewed by construction, 2026-08-01.* The MVP client and deck.gl viewer
(`2026-08-01-mvp-client-and-deckgl-viewer-design.md`) was built and run against the 2.4 × 10⁶,
10⁸ and 10⁹ fixtures. Four annotations of that date come from that exercise rather than from
reading: §8.2's "a stranger gets a working map" claim, falsified at 10⁹ by one tab shedding 12 of
23 requests; §10's budget gap, which is a depth-choice problem before it is an allocation one;
§9's underlay resolution, which cannot relieve mark-count-as-density while it is blockier than the
marks; and design §7.2's knobs, none of which expresses marks-on-screen. **The method is worth
recording as much as the findings: none of the four was visible in the document, and three of them
were invisible below 10⁸.**

*Draft reviews (one agent, four passes).* A conformance pass against the design
documents and invariants, which corrected the secrecy/truthfulness division, the
claim-minting-proxy gap, and two bugs in §5; a landscape survey of consumers,
alternatives and access-control prior art; a primitive inventory under the corrected
test; and the four-seam integration study which produced §2's two-channel anatomy,
§8.6's tile-addressed alias, and the Mosaic refusal.

*Wide-net review (second agent, fresh eyes, 2026-08-01)*, commissioned deliberately from
someone who had not produced the material — the owner's brief was to attack the frames
and bring ideas rather than refine prose. Both organising frames survived. It added
§5.1's server-held session cursor and its game-server prior art, §6.1's live-data
behaviour, §7's T2(c), §15's fused-column variants, and the fourth display state; and it
corrected two things this document had asserted — §2's consistency framing, which
over-dramatised a problem that reduces to snapshot isolation, and §10's claim that the
conformance kit binds clients we cannot inspect, which it cannot. It also proposed four
scope cuts (deferring the elision mechanism and §12's extension API, dropping the
deepscatter build, and promoting the WASM kernel); the owner ruled to **keep scope** on
all four, since the document builds nothing and re-deriving a decision costs more than
recording it, and to keep the WASM kernel as an open question only.

Owner rulings recorded in place: staleness is acceptable and the boundary is the
request, not the pixel (§4); encodings are configuration first (§9); drill-down is a
cursored region rather than a sixth verb; filters are content-addressed rather than
handled; one core with Python embedding it (§10); the analytical grammar is deferred but
not foreclosed (§14); and the integrations are designed before the architecture, which
is the method that produced §8.

Facts in §8 marked as verified were checked against current upstream documentation on
2026-07-31; deepscatter's manifest details and MapLibre's current major were not, and
carry that caveat. Per viz architecture §5, verify licences and capabilities before
committing.
