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

**The obligation this exposes is cross-channel epoch consistency**, and it is the
single most load-bearing thing a client does. Nothing in any renderer prevents drawing
marks from one epoch beside numbers from another — a cached tile under a fresh count,
or a stale count beneath fresh marks. A mixed-epoch display is exactly the
confidently-wrong failure this side of the system exists to prevent, and **only a
client can enforce it**: the service answers one request at a time and cannot see the
screen. It is invisible to a naive integrator until it bites.

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
is a defect to design out, most cheaply by having the client library own *k* and never
expose it, not an obligation to document.

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

### 8.1 The partition that comes first

**Integrations divide by whether the consumer can hold the visible set.**

*Resident* integrations — Mosaic, DuckDB, Embedding Atlas, pandas — are bounded by the
**principal's visible-set size**, not by corpus size, and give exact local aggregates
over `M_auth`. They are notebook-scale by construction.

*Streaming* integrations — deck.gl, MVT, our own core — are bounded by **screen area**
and scale to any corpus and any principal.

The design's central cost claim, *cost scales with screen area rather than corpus
size*, **holds only for the second class.** An integrator can self-select in one
sentence, and the export threshold (§8.4) is not an arbitrary limit but the boundary
between the classes: above it the honest answer is "use a streaming integration", never
a truncated table.

### 8.2 deck.gl — the control case *(verdict: achievable now)*

Seam: `TileLayer.getTileData` plus binary-attribute sublayers. Demands **zero new
server surface** — `meta` and `viewport` suffice today; labels join at Phase 3.

Four findings worth carrying:

*The composition viz §9 flagged is documented, not merely plausible.* Current deck.gl
docs specify non-geospatial tiling: x and y increment from the world origin, each
tile's size matches `tileSize`, `bbox` arrives as `{left, top, right, bottom}`. The
risk narrows from "does this exist" — which viz §9 said would reopen the Embedding
Atlas decision — to "does the arithmetic line up": y-axis orientation, the zoom→z
mapping, and refinement under real sublayers. **The smallest discharging spike contains
no Tessera at all**: ~50 lines of orthographic view plus tile layer with a synthetic
`getTileData` drawing each tile's index and bbox, asserting index arithmetic at z 0–16,
y direction, abort-on-fast-pan, and cache behaviour. Half a day.

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

Two frictions to record rather than discover. The x/y **interleave**: `getPosition`
wants interleaved pairs and we ship separate `x`/`y` columns, so the core does an
O(served) pass — noise at small *k*, an ~80 MB shuffle per refresh at 10⁷ marks. An
Arrow `fixed_size_list<f32,2>` position column would be zero-copy, and is GeoArrow's
point encoding; a candidate **additive** wire change to decide on P2's numbers. And
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
   the resident class, and the SQL surface is not a substitute for it.
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

## 10. The client stack

**One headless core, in TypeScript**, owning everything invariant-bearing: session and
token lifecycle, viewport-to-range arithmetic, tile scheduling and prefetch, Arrow
decode, the replica state of §3 and §5, filter state, frontier and label selection,
cross-channel epoch consistency (§2), and *k* (§4, P6). Viz architecture §1 already
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
non-decreasing, the three-state trichotomy, cross-channel epoch consistency. It is what
binds mode-3 clients we cannot inspect, and §11's determinism is what makes it
shippable.

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
conformance kit. Its unit of adoption is a documented stable verb, not a feature — so
the cheapest adoption wins are documentation, not engineering.

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
- **Whether position storage should be Morton-residual rather than `x`/`y`** *(owner,
  2026-07-31)*. §2.6 stores `x`, `y` as `float32` "as supplied (quantisation is for
  codes, not storage)", so `morton.u32` duplicates their high bits: what Morton cannot
  recover is only the residual *within* a cell. Storing `morton.u32` plus an interleaved
  32-bit residual is 8 B/row against 12 B — **4 GB at 10⁹**, the same magnitude and the
  same argument as r5's narrowing. Three caveats: the per-viewport **gather is roughly
  neutral** (8 B either way, since Morton is not currently read per served row), so the
  win is residency rather than hot-path page traffic; a 16-bit residual gives only 256
  sub-positions per cell, which bands visibly at depth 16 where a tile *is* one cell, so
  32 bits is the safe width; and it requires ruling that positions are stored
  fixed-point rather than as supplied, which is a contract change the oracle inherits.
  The wire need not change — the server dequantises during the gather, and that pass can
  absorb the interleave the item above wants. **Belongs to the drawn-mark-budget spec and
  P2**, not to this document; recorded here because it was raised during this design.
- **Licence review** for any Grafana or Metabase plugin work (both AGPLv3: a plugin is
  standard practice, embedding or forking the host is an AGPL event).
- **Multi-slice comparison** has no client-architecture position yet.
- **Control-plane client story**: build-credential custody in a notebook, given that node
  iteration must never be reachable from a user token (§2.5).

## 16. Provenance

Brainstormed with the owner 2026-07-31. Reviewed twice in draft by an independent agent
with no stake in the plan being right, per CLAUDE.md's working method: a first pass
against the design documents and invariants, which corrected the secrecy/truthfulness
division, the claim-minting-proxy gap, and two bugs in §5; a landscape survey of
consumers, alternatives and access-control prior art; a primitive inventory under the
corrected test; and a four-seam integration study which produced §2's two-channel
anatomy, §8.6's tile-addressed alias, and the Mosaic refusal.

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
