# Client interaction architecture — how anything talks to Tessera

**Date:** 2026-07-31
**Status:** design, for review
**Companion to** `tessera-visualisation-architecture.md`, which owns *rendering*; this document owns everything protocol-facing.
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

Every seam analysis in §8 falls into this shape: *renderer via the seam, product via
the verbs.* An integration that uses only the mark channel produces a picture with no
trustworthy quantities in it — which is a legitimate product (a viewer) but must be
named as one.

**The obligation this exposes is cross-channel epoch consistency** — but it is smaller
and more mechanical than the split makes it look, and an earlier draft over-dramatised
it *(third review, 2026-08-01)*. Within one response the channels are atomically
consistent **by construction**: the viewport verb delivers tile counts and points in a
single body (contracts §5). The risk is purely *temporal* — composing responses fetched
at different times, so that a cached tile sits beneath a fresh count or the reverse.

So the obligation reduces to **render only responses sharing one epoch**, which is
snapshot isolation, and it is therefore a **data-structure property of the replica
store** rather than a discipline every integrator must hold: all state keyed by epoch,
the renderer reading exactly one epoch's keyspace, flips atomic. That reduction is what
makes it testable — the conformance assertion is one predicate, *no frame mixed
epochs*, rather than a review of everything a client draws. Only a client can enforce
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
A consumer that ignores reconciliation, epochs, prefetch and every optimisation gets
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
*don't re-request* needs an explicit staleness bound, and the bound is the epoch.

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
it: `served(viewport)` is stable within **(mask, overlay version, slice, k, identity
epoch)**. §7.2 accepts θ movement on overlay swap; contracts §2.6 makes row order
key-dependent. Those five coordinates are the epoch (§6).

**The mechanism.** §7.2 defines `served(T)` as the smallest `m(T)` members of `vis(T)`
by `tessera_id` — the served set is a **`tessera_id`-order prefix** of the visible set
in a range, the union of floor, threshold and cap being a prefix is what the nesting
proof turns on, and the client can evaluate that ordering itself because it holds the
identities. So client state is declarable as **prefix declarations** — *for tile T I
hold everything up to cut c, as of epoch E* — rather than identity lists. One parent
declaration answers for all four children on zoom-in, which is the case a tile-level
ETag cannot cover.

Three properties, in the order that matters:

*Fail-closed by construction.* The server names the complete served set; membership is
never inferred client-side. A suppressed item is simply not named, so the client drops
it. No invalidation protocol exists to get wrong.

*Advisory.* A server ignoring every declaration is correct (P5). That is what makes it
reviewable, and it is why the epoch guard suffices for the one real bug: an item
flushed or unsuppressed below the client's cut would otherwise be assumed-held and
arrive with no attributes, so a stale epoch simply drops the declaration.

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

**Deferred pending a written safety argument: epoch-delta naming** — naming only
changes since the client's epoch rather than the complete served set. It fixes the
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
drawn-mark spec's handle tables. A **per-session cursor** — at minimum the last epoch
answered, at most the tile→cut record mirroring what it named — is a small addition to
state that exists anyway, and it changes the safety calculus above. The fail-open risk
in delta naming is trusting a *client's claim* about what it holds; if the server
computes "changes since epoch E" from its **own** record — the deny set since E, which
contracts §2.3's side-manifest rule already publishes immediately, plus what became
visible since E — then no client claim is load-bearing and the completeness guarantee
becomes checkable in one place.

Tessera's version of this record is far cheaper than the field's, and for a reason
specific to this design: because `served` is a `tessera_id`-order prefix, `(tile, epoch,
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

## 6. The epoch, and the change signal

All four seams independently demand a client-visible unit of cache validity: MVT needs
epoch-scoped URLs for safe browser caching, deepscatter needs reload scoping,
Mosaic-extract needs a consistent snapshot, deck.gl needs an `updateTriggers` key. So
the epoch is an **integration requirement**, not an internal optimisation.

The epoch is the five coordinates of §5. Distinct from `x-tessera-pin`, which is
row-space geometry and deliberately not authorisation state (I11).

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
(`410`), overlay version, identity epoch (`409`), and key rotation each void a
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
tick on a configured cadence of order seconds, batching epoch advances. That kills by
construction the failure this section should fear — epoch churn under continuous ingest
driving constant refetch — and it means SSE, long-poll and plain polling are all
adequate. SSE is the mild favourite because `Last-Event-ID` gives epoch-resume for free
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

1. *Refresh is a new epoch snapshot, not an in-place update.* Fetch behind the current
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
3. *Cache validity binds to the epoch integer.* A tile older than the last signalled
   epoch is renderable but **stale-marked**, and no number-channel value may be
   displayed against it. One integer comparison, and it is what closes §4's
   "pan answered entirely from held tiles" corollary with a mechanism rather than a
   remark.

### 6.2 What the epoch is made of, and what each part invalidates

Treating the epoch as one monolithic key is over-coarse, and an earlier draft did
*(owner, 2026-08-01)*. Its components invalidate genuinely different things, and folding
them together forces a full re-render for a change whose delta is tiny.

| Tier | Advances on | What it voids for a client |
|---|---|---|
| **Identity generation** | key rotation, identity-epoch advance | **everything** — every held `tessera_id` becomes meaningless and row order changes with it |
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
steps; a revocation denylist, which is the same class of machinery as the epoch ledger;
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
epoch refresh maps directly onto it) is the strongest candidate for mode 2's table and
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
coalescing, the multi-stream framing, cross-channel epoch consistency, `{shown, total}`
discipline, the *k* obligation, three-state rendering and re-authorisation. **So the TS
core is load-bearing for conformance and consistency, and merely convenient for
everything else** — which argues for shipping the obligations list and conformance kit
as first-class artifacts in their own right, not as documentation of the core.

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
epoch-scoped URL segments using a session nonce and never the bearer token, and a
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
per-epoch, re-taken on the change signal, and all interaction is local.

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
epoch-scoping tile URLs so a reload cannot mix epochs, and accepted under §4's staleness
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

1. **The epoch** as a client-visible unit of cache validity — all four, independently.
   Promoted to architecture in §6.
2. **A `tessera_id` representation rule**, because u64 is a JS-ecosystem liability with
   three distinct answers: BigInt on Arrow paths; a decimal string in JSON; and
   presentation-local ordinals minted inside the trust boundary where a seam's identity
   slot is too narrow (MVT feature ids, deepscatter's `ix`). Binary-attribute renderers
   need none of it — identity never enters their render path.
3. **A tile-addressed GET alias of the viewport verb.** Three of four seams address data
   *by tile*; our verb is viewport-addressed. An alias with the epoch in the path and a
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
their signature interactions are currently unaddressed.

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
multi-decade distributions and beats a fixed log transfer. **Selection should become a
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
*(third review, 2026-08-01)*: **shown-but-stale** — drawn from epoch *E* while the change
signal reports *E′ > E*. The ruling makes this legitimate; nothing currently makes it
*visible*, and an unmarked stale display is the truthfulness failure the staleness
concession quietly buys. The fields that handle this honestly are the regulated ones —
delayed market data must carry a delay badge — and the general pattern is an "as of"
affordance owned by the core and surfaced by default rather than opted into. It belongs
beside the trichotomy as a conformance item, and it is the display half of §6.1's rule
3.

### 9.1 Derived artifacts, and the three ways they are gated

Everything above concerns points. **Clusters, labels, hulls, contours, aggregation
cells, edges and trajectories form a second class** — artifacts derived from points,
whose visibility is tied to point visibility — and the design carries two instances of it
with different rules and no framework connecting them *(owner, 2026-08-01)*. Labels
(§7.6) gate on exact containment; cluster nodes (§7.5) gate on a threshold; C2 records
that node extent and hull need no gate at all. The generalisation is worth stating,
because a third artifact type will otherwise go looking for precedent and find two
contradictory ones.

**The rule: a derived artifact is gated by how it was produced.**

| How produced | Gate | Instance | Literature |
|---|---|---|---|
| Shared, precomputed, content-bearing | **exact containment** — serve iff `G ⊆ M_auth` | labels (§7.6), annotations, region summaries | the derivation axiom, multilevel-secure databases (Appendix D) |
| Shared, precomputed, structure-revealing | **threshold**, as a named disclosure control | cluster nodes, `min_visible_members` (§7.5) | **small-cell suppression**, statistical disclosure control |
| Recomputed per viewer from masked membership | **none** | hulls, centroids, contours, cells (C2) | not a shared artifact, so nothing to gate |

Content-bearing artifacts need containment because their *content* can reveal what is
inside documents the viewer cannot read — and they inherit §7.6's availability
pathology, where one deletion breaks containment for every principal and a nested chain
dark-ships. Structure-revealing artifacts disclose only existence and shape, so a
threshold is defensible, but only as the security control §7.5 already insists it be.
Per-viewer-recomputed artifacts are safe by construction, and the failure to avoid is
**serving a generated artifact instead of recomputing it**: a build-time centroid over
full membership, shown to someone who sees 5% of it, points at where the invisible
members are.

**What the class actually is** *(owner, 2026-08-01)*. A cluster has no inherent geometry:
each point is a member of some cluster set, and the cluster's shape is *derived* from that
membership. Generalising — **every member of this class is a named subset of the point
set, plus an attachment**:

| Artifact | Subset | Attachment |
|---|---|---|
| Cluster node | membership bitmap | geometry, **derived** per viewer |
| Boundary polygon | points inside it, resolved at build | geometry, **supplied** and corpus-independent |
| Label | generating set | content, **derived** from the corpus |
| Aggregation cell | a Morton range | nothing — the range is the subset |

The engine already has this machinery: §7.5 specifies a membership bitmap per node plus a
bounding box per slice for pruning, with geometry recomputed from masked membership. So
the class needs no new mechanism, only a gate.

**But a polygon may be independent of the points, and that splits it off the table above**
*(owner, 2026-08-01)*. A cluster cannot exist without members; a boundary can. Three
distinct things hide here, and only the first belongs to this class:

*A geometry that **induces** a subset.* A postcode exists whether or not any document
falls in it, so it is not a subset with an attachment — it is a shape whose relation to
the point set is derived, and possibly **empty**. It joins the class through its induced
count, not through its existence.

*A geometry used purely as **context**.* Reference outlines the client draws. Viewer-
independent, disclosing nothing, contributing to no displayed quantity. Free.

*A polygon that is an **access-controlled item in its own right***, with its own identity
and its own terms. This is **not a derived artifact at all** — it is an item that happens
to have an extent, governed by the points framework rather than this one. At the
cardinalities in question it is cheap: give it an entity ID and a code from its containing
cell, and mask it exactly as a point is masked. The extent matters only for tile
assignment, which the smallest-containing-tile convention handles, and at 10⁴–10⁶ objects
a full scan per viewport is defensible anyway.

**The trap sits between the first two, and it looks like no decision at all.** If a client
draws only the boundaries that contain visible points, **that filtering is small-cell
suppression with a threshold of one**: displaying a boundary asserts "at least one visible
item here", omitting it asserts "none". One is precisely the threshold the census
literature identifies as too low. So either draw **all** boundaries — context,
viewer-independent, free — or gate them on `min_visible_members` like every other
structure-revealing artifact. Gating on non-emptiness is the option that must not be taken
by default.

**The two kinds coexist, and mixing them needs no new mechanism** *(owner, 2026-08-01)*.
A deployment may hold boundaries tied to point visibility by a threshold *and* boundaries
under independent term-based control — and the same boundary may be both. If a boundary
carries its own terms it is an item, so it lives in entity space and the existing mask
covers it: one token, one satisfied-term set, two populations. `M_auth ∩ boundary_ids`
gives the boundaries a viewer may see; `and_cardinality(members(B), M_auth ∩ point_ids)`
gives the masked count within one. The induced-membership relation is §7.5's node
membership bitmap under another name.

Two composition rules follow.

*Terms first, always.* The boundary's own mask decides whether the viewer learns of it at
all; the threshold applies only after. **The fail-open to name is the reverse** — a
healthy induced count surfacing a boundary whose terms the viewer does not satisfy, which
is contained data granting access to its own container. Conjunction, never disjunction,
in the same shape as `M_sel = M_auth ∧ filters`.

*Where the geometry is independently authorised, the threshold governs the count, not the
shape.* For a cluster the hull **is** corpus-derived, so a threshold must suppress the
geometry — the shape is the disclosure. For a boundary the viewer is cleared for, the
shape discloses nothing they are not already entitled to, so withholding it achieves
nothing and costs the map: what must be withheld is the **number**. That is §7.5's
rollup-rather-than-suppression applied to a second object — a boundary with no count, or
a count at a coarser level of the administrative hierarchy, rather than a hole.

**And the gate is chosen by whether the attachment is corpus-derived**, which is sharper
than "how it was produced". A city boundary exists independently of the data, so its
*shape* needs no gate at all and only the count within it is masked. Label text derives
from documents, so it takes containment. A cluster hull derives from membership, so it is
recomputed rather than gated.

**A second, orthogonal axis: disclosure sets the gate, cardinality sets the mechanism.**
Clusters and labels top out around 10⁵; boundary polygons are similar — *"city, town,
postcode level, rather than per point"* (owner) — so all three are **artifact-scale**,
where a per-item test against the mask is affordable and membership bitmaps are the right
representation. **Edges are the exception and are point-scale or larger**: a sparse graph
over 10⁹ points is 10⁹–10¹⁰ edges, so a per-item containment test is impossible however
correct it is, and edges need the points' machinery instead — an ordering, contiguous
ranges, bitmap arithmetic, and a priority prefix. Structurally, with edges sorted by
`(source, target)`, the visible set is the adjacency runs of visible sources intersected
with visible targets: O(visible edges), bounded by the mask rather than the corpus. **The
gating rule for edges is right and the mechanism is not** — that is what the cardinality
axis catches.

*Polygons, consequently.* Rare boundary polygons need **no new spatial index** for the
query that matters: "how many points in this postcode" is a build-time membership bitmap
and one `and_cardinality`, with no query-time spatial join. Only "which polygons intersect
this viewport" wants a structure, and at these cardinalities that is the bounding-box
prune §7.5 already performs for nodes. Administrative boundaries are also naturally
hierarchical, so the frontier machinery applies unchanged. *Context* polygons — reference
outlines the client simply draws — need none of this, and the only rule they carry is that
they never contribute to a displayed quantity.

**The threshold tier has a fifty-year literature and a named attack we have not checked
against** *(survey, 2026-08-01)*. `min_visible_members` is **small-cell suppression**
from statistical disclosure control — the census-table field — and that field's central
known weakness is the **differencing attack**: two overlapping releases whose difference
isolates a cell below the threshold. §8.4 already blocks the filter route by fixing
maximum depth against `M_auth` rather than `M_sel`, which is the operational form of I12.
**What has not been examined is differencing the frontier across pan, zoom and slice.**
C1's owner review is listed as outstanding before launch; it should be conducted in that
vocabulary and against that literature rather than from first principles. Raised as an
annotation at design §7.5, since the control lives there.

**The differentiator check came back clean.** Nobody gates shared precomputed derived
artifacts per viewer. The field has exactly two other moves, and both are the near
misses: *re-derive per query under a filter* — Elasticsearch's `geotile_grid`/`geohex_grid`
under document-level security, which is the nearest thing to our underlay that exists in
production, at per-query cost and with the seam leaks its own documentation concedes;
and PostGIS `ST_ClusterDBSCAN`/`ST_ConcaveHull` under row-level security, possible but
O(corpus) per query and heir to the query-plan disclosures Appendix D demolishes — or
*regenerate per viewer and never share*, which is the whole permissions-aware RAG
pattern, avoiding the gating problem by paying generation cost per viewer per query.
**Nobody does the third thing: share the expensive artifact and gate it with a cheap
containment test.** That is §7.6's "appears unpublished" claim, now checked against the
adjacent fields rather than assumed. §7.5's rollup-rather-than-suppression frontier
likewise has no analogue found in any clustering or mapping system — an absence claim,
and marked as one.

**Hierarchies ride the verbs, not the tiles, and the API idiom already exists.** There is
no wire standard for delivering a cluster hierarchy; MVT is flat per tile, and the maps
industry encodes hierarchy as per-zoom membership plus `rank` properties. What *is*
de-facto standard is an interaction API — supercluster's, which every mapping developer
has met: `getClusters(bbox, zoom)` returning flat features with `cluster_id` and
`point_count`, plus `getChildren`, `getLeaves(id, limit, offset)` and
`getClusterExpansionZoom`. That maps almost one-to-one onto what Phase 3 will build:
session node handle for `cluster_id`, masked count for `point_count`, the keyset cursor
for paged leaves, frontier depth for expansion zoom. **Speak that idiom.** Its
architecture — a KD-tree over fully resident data — is unavailable to us and irrelevant;
the API shape is the transferable part. The MVT adapter accordingly emits the current
frontier as flat per-zoom features carrying rank, exactly as basemap schemas do.

**Contours: serve nothing.** They are a client-side derivation of the number channel —
marching squares (`d3-contour`) over an aggregation grid — and our underlay *is* that
grid, already masked and exact. Client-derived isolines over served sub-cell counts are
per-viewer and correct with **zero new server surface**, and level selection is a client
encoding under §9's ruling. This generalises the third tier: **any client-derived geometry
over served masked aggregates is safe**, because it derives from what the viewer can
already see.

**Edges are the benign case of containment, and their hard problem is elsewhere.** An
edge's generating set has cardinality two, so exact containment is cheap and free of the
label pathology — dark-shipping pain scales with `|G|`, and one hidden endpoint correctly
kills one edge rather than a chain. Delivery convention is uniform across the graph
renderers: **edges travel as index pairs into a node buffer**. The collision is not with
masking but with **sampling**: an edge is drawable only if both endpoints are in the
*served* set, not merely the visible one, so a future graph domain must either restrict
edges to served×served — degree-biased, and a named hard problem, the **induced-subgraph
sampling problem** — or let edges pull their endpoints into the served set, perturbing
the point sample. Parked with that name; Appendix H's masked-degree aggregates need none
of it and remain the near-term graph story.

**And this corrects §2's naming.** A cluster hull is an aggregate over the visible set, so
a client that draws a hull around the *k* points it holds has committed the sample-as-set
error in geometry rather than in numbers. **The number channel is really the exact
masked-aggregate channel, and geometry travels on it** — hulls, centroids, contour inputs
and cell counts alike. The mark channel carries the sample; the other carries whatever is
exact, whatever its shape.

**Three traps, one sentence each.** Adapter cluster and cell tiles must never be cached
across viewers — the same rule as point tiles, restated because aggregate tiles *look*
shareable. Cluster identifiers in the wild are ephemeral per rebuild, which matches our
per-session node handles, so promise no more stability than supercluster taught people to
expect. And any client-side derivation that computes breakpoints "from the data" means
*the viewer's masked data* — automatic in our model, but state it, so nobody imports a
library preset that expects corpus-global breaks.

*(One presentational idiom worth copying, from Kibana Maps: a "blended" layer that
auto-switches between individual documents and cluster marks on a count threshold. That
is the marks-to-underlay transition, decided client-side from served counts.)*

## 10. The client stack

**One headless core, in TypeScript**, owning everything invariant-bearing: session and
token lifecycle, viewport-to-range arithmetic, tile scheduling and prefetch, Arrow
decode, the replica state of §3 and §5, filter state, frontier and label selection,
cross-channel epoch consistency (§2), and *k* (§4, P6).

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
marks a client declines to *draw* is presentation; which marks it is *served* is I7. Viz architecture §1 already
fixes this boundary; this document fills in its protocol half.

**The core is a first-class distributable, not a reference implementation.** It is the
recommended path for anything running in a browser, whoever's frontend that is — which
narrows the population that cannot use it to non-JS consumers. §8.2 measured what it is
worth: a stranger gets a working map without it and a *conforming* one only with it or
with a reimplementation of the obligations list.

**Three layers, each usable alone**, because the three usage modes want different
surfaces: a **session client** (plain async verb calls, no state — what a REST user
would write anyway); a **replica store** owning cache, epochs, reconciliation,
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
non-decreasing, the four display states, and §2's one-predicate epoch assertion. §11's
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
server-side randomness. Three payoffs nobody has claimed: bit-exact cache-correctness
tests; a **record-replay conformance harness** (a scripted server with canned
suppressions mid-session, asserting displayed state) which is how §10's kit becomes
operational against clients we cannot inspect; and cross-session reproducibility as a
documented feature for notebook users — same credentials, same corpus, same picture.

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
actionable consequence for now is to **generalise the sampler's branching factor**
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

**Epoch-delta naming.** Not rejected — deferred pending a written safety argument. §5.

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

  The **gather is neutral between today and A** — Morton is not currently read per
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
  already owns, plus the nesting and *k* rules and epoch comparison — precisely the code
  where an engine/client disagreement would be silent and conformance-relevant. A small
  crate compiled to WASM would give one implementation. The boundary matters if it is
  ever taken: **arithmetic only**, with the replica store staying TypeScript, because
  chatty stateful APIs across the WASM boundary are where Rust-in-the-browser goes wrong.
- **Is the epoch a readable coordinate, or only a cache-busting nonce?** *(third review,
  2026-08-01)*. Every sync engine surveyed answers "readable, with a retention window".
  It matters at §6.1's flip: a pan mid-flip may need one more *old*-epoch tile to keep the
  outgoing snapshot complete. If the server will serve a still-retained epoch — the same
  retention shape as pins and `410` — flips never tear; if not, the client force-flips
  early or shows holes. Serving a stale epoch briefly keeps an accepted suppression
  visible within that epoch's responses, but bounded by the same deny-visibility budget
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

Brainstormed with the owner 2026-07-31. Reviewed by independent agents with no stake in
the plan being right, per CLAUDE.md's working method.

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
