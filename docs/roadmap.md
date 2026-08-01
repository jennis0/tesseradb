# Roadmap

**Status:** Non-normative. This document records what constrains the order of work. It does not
record status — the GitHub issues are the sole authority on that, per
[`agents/epic-lifecycle.md`](agents/epic-lifecycle.md) — and it does not specify anything. Where
it disagrees with [`design/`](design/), the design wins.

Work here is organised as capability blocks rather than phases, and that stays true: nothing
below is a date. What this document adds is the layer above a single epic — which capabilities
belong together, which cannot start before another finishes, and which are only *apparently*
independent because they amend the same document or the same byte.

Two releases, and a third list of things recorded but not scheduled.

---

## 1. The two releases

**Release 1 is the first time someone outside this project relies on the claims.** That framing
decides most of what it contains. A deployment must be able to express a real access policy, load
its data, keep it current, filter it, see structure in it, and be told plainly what it is
accepting — and the claims it rests on must be checkable by something other than reading the
code. Capability breadth is not the constraint; verifiability is.

**Phase 2 is reach.** More ways in (standard geospatial tooling, in-process embedding), more ways
to search (text, vectors), more machines, and the compartment boundary. Each is a capability the
system does not need in order to be honest about what it already does.

The third list is speculative — recorded so it is not rediscovered, and scheduled by nobody.

---

## 2. Release 1

**Release 1 is a spine with four lanes beside it, not a fan.** The gate comes first: three epics
wait on it and a fourth is gated by it. The slices fold-in follows, because most of its cost is
rework that is avoided by taking it early and paid for by taking it late. Ingest, caching and the
client all build on the table addressing that fold-in settles, and documentation is last by
construction because it describes what the others produce.

Running independently alongside that spine: authorisation and sessions, attributes and their
filters, clusters and their labels, and [#56]. The reference viewer ([#46]) can slot in anywhere.

### The gate

> [#11 Prove the invariants hold: complete the conformance suite and run it in CI][#11] ·
> [#45 Measure performance continuously and block regressions][#45] ·
> [#14 Keep the specification true against the system][#14]

The conformance suite is the deliverable. An implementation that keeps Morton ordering, Roaring
masks and tiered decode while quietly dropping I2, I7 or I13b passes every functional test and
leaks; the suite is what distinguishes the two, and at release it is the only thing standing
between a security claim and a reader's trust in it. Three of thirteen invariants are covered as
designed. Six have no coverage.

All three epics share one missing prerequisite: **there is no CI**. [#11] says so, [#45] says so,
and [#14] says its two checkers do not run automatically *because* of [#11]. Nothing else in the
release is blocked by CI's absence, and everything in the release is worth less without it. It is
the first thing.

Two other epics reach back into this theme. [#11] cannot close its I5 coverage until [#6] ships a
non-trivial plugin, and the slices epic has a working suite as its stated adoption gate — so the
gate is not scenery around the release, it is inside it.

### Slices and signature grouping

> [#48 Carry several coordinate systems over one corpus][#48] ·
> [#49 Group rows by permission signature to cut authorisation cost][#49] — **merged**

One epic, because they are one mechanism. A table is addressed by slice, group and flush; [#48]
is the slice component and [#49] the group component, and sequencing them apart means building
the addressing twice. Slices give a corpus more than one coordinate system over one identity
space; grouping stores items that share a permission signature together, which is the
highest-leverage layout property available — bitmap operations cost time proportional to blocks
touched rather than items matched.

The grouping half is measured but not built, is off by default, and its adoption gates are stated
in the design: a working conformance suite including the byte-level single-vs-multi-table
differential build, the fan-out sweep run to its knee, and a real signature histogram — the last
of which is deployment guidance this project cannot produce.

**This epic constrains more of the release than any other, and its cost is almost entirely a
question of when.** Three reasons to take it early:

- **`segment` becomes `table`.** The design rewrites the concept to `(slice, group, flush)` and
  amends architecture §11.2 and §13, contracts §2.1, §2.3 and §2.6, and lifecycle §3 and §5 to
  match — including giving the move deny its own named retirement rule. [#3], [#4], [#8] and [#9]
  are all written against `segment`. Folding this in after they are built is rework of machinery
  that carries invariants, in the one part of the system where a conflation has already been
  caught fail-open twice.
- **A window closes when the clients ship.** `slices-and-multi-table.md` §12 lists six things
  cheap now and expensive later — container-aligned table base offsets, the manifest `group` key,
  prefix-qualified table references, the `{slice → (x, y)}` ingest map, resolving contracts §2.6's
  *"row IDs are segment-local"* to the slice-global reading, and keeping the permutation behind
  the reader interface. The ingest map is free **only while `api_version = 1` has no published
  reader** — and [#10] and [#47] exist to publish one, in this release.
- **Its fold-in is seven documents' worth of rulings.** §11 of the design proposes amendments to
  architecture, contracts, lifecycle, system architecture, conformance and Appendix C, each an
  owner decision on its own. That is lead time, not work, and it does not parallelise.

The design is provisional pending exactly that fold-in.

### Ingest complete

> [#3 Make ingested items visible without an offline rebuild][#3] ·
> [#4 Retire denies correctly: the epoch ledger, the suppression rule and the compaction fold][#4] ·
> [#8 Add a batch of items to a running deployment][#8]

Today an acknowledgement is a durability receipt, not a visibility promise, and the gap between
them is unbounded — visibility waits on an offline rebuild. [#3] closes that with flush. [#4]
builds the deny machinery that a mutable deployment needs: three retirement rules that retire
three different ways, of which **two are specified and unbuilt**, safe today only because nothing
retires at all. Compaction belongs to [#4], because the fold is what compaction is for in
invariant terms. [#8] then lets a batch enter a bundle that is already serving, which presupposes
[#3] — landing into a *running* deployment means nothing until publication makes items visible.

The theme's difficulty is identity rather than throughput. Entity identifiers are append-only,
assigned in signature order, and that ordering is scoped to a batch — so batch boundaries are
permanent features of the index. [#8] opens with a design decision recorded in the specification
as an open question: whether a batch enters an existing bundle appended, merged or staged. It
determines everything downstream of it, and it is a ruling before it is work.

This whole theme is written against `segment`, which the slices epic renames — see above.

### Authorisation and sessions

> [#6 Ship reference authorisation plugins for OIDC and role-based access][#6] ·
> [#55 Bind sessions to an idset with a signed token][#55]

The differentiator is the access control. The only plugin that ships is `Passthrough`, which
splits a string on commas, and the WebAssembly sandbox does not exist. A release on that footing
asks every deployment to write its own plugin against an ABI nothing but passthrough has
exercised.

[#6] carries a second job. I5 — that the two authorisation functions agree about what a term
means — is identified in the specification as the system's largest unverifiable dependency:
nothing downstream can check it. Under passthrough both functions are the same string comparison,
so I5 is trivially true and cannot be tested. A non-trivial plugin is the precondition for
testing it at all, and therefore for [#11] closing.

[#55] makes a key rotation invalidate sessions by itself rather than by a sweep, on
[decision 0025](decisions/0025-rotation-is-a-session-invalidation-event.md)'s ruling that a
rotation is a session invalidation event. **It has to be sequenced with the slices fold-in**,
which separately proposes splitting rotation into *roll* (multi-idset key retention, identifiers
translate, no break) and *revoke* (today's 409 semantics) and weakens C17's time-bound as the
point of the change. Two epics redefining what a rotation means: one decision, taken once, or
[#55] is built against a definition the same release has already changed. [#55] also requires I6
to be amended through its own review — it currently reads that the service never infers, looks up
or refreshes credentials, and retrieving a visible set by reference is literally a lookup.

### Attributes and filtering

> [#42 Carry per-item attributes alongside the point][#42] ·
> [#43 Filter by category, time and number][#43]

An item becomes more than a coordinate. [#42] lets a deployment declare per-item columns at build
time and serves them; [#43] makes them filterable, and needs [#42] first because the service
advertises an empty operand list until something populates one. The build currently always writes
an empty column list, so the schema's attribute tail has never been non-empty in practice.

The shape is deliberate and worth preserving under pressure: filters are order-independent set
producers composed by intersection. The leak register can be exhaustive because there are about
five retrieval shapes; a general expression endpoint could not be enumerated that way. New
capability enters through the filter contract (§8.2) so that expressiveness never reaches the
authorisation layer.

[#42] declares its columns in the manifest at contracts §2.2, and the slices epic makes
hot-column enumeration per-slice at §2.3 — adjacent sections, one schema, one release. Cheap to
reconcile as one change and tedious as two.

### Structure on the map

> [#13 Serve cluster structure computed from the viewer's own visible set][#13] ·
> [#41 Serve cluster labels only to viewers who can see what generated them][#41]

Points carry cluster structure and clusters carry labels, both computed from the viewer's own
visible set. This is where the system stops being a scatterplot. None of it exists — no cluster
tree, no frontier, no membership structure — which is why I3 and I8 have no tests. [#41] follows
[#13]: a label attaches to a cluster node.

The disclosure question is the whole difficulty in both. A cluster boundary computed over all
items and then filtered tells a viewer about items they cannot see; a label derived from items a
viewer cannot see is a statement about those items. Membership, extent and hull are recomputed
per viewer from masked members only, and labels gate on exact containment against `M_auth` and
never against the filtered mask.

Both wait on [`design/derived-artifact-gating.md`](design/derived-artifact-gating.md) becoming
normative. It exists because three artifacts were gated by three rules in two sections with
nothing connecting them, and a fourth would go looking for precedent and find contradictory
answers; promoting it is what makes these two decidable rather than improvised. It also governs
[#43]'s frontier behaviour and phase 2's [#12].

One known revisit: [#12] requires cross-partition containment to merge correctly, so a router
ignorant of an unreachable slice does not serve labels it should withhold. With [#12] in phase 2,
[#41] will be built against the single hardcoded partition that exists today. Correct now, and
deliberately so.

### Client and server performance as one problem

> [#9 Keep the right state resident on the server, and evict safely][#9] ·
> [#10 Make the JavaScript client a library others can depend on][#10] ·
> [#46 Publish a reference viewer that shows a correct client][#46]

What a viewer experiences is a property of where data rests, and that spans the boundary: [#9]
owns what stays resident on the server and how it is evicted, [#10] owns the published JavaScript
library and its cache, [#46] is the reference viewer that demonstrates what a correct client
owes.

**These are three tiers of one model, not three caches.** `caching.md` §5 specifies seven: S1 and
S2 are [#9]'s, C1 and C2 are [#10]'s, and S5 belongs to phase 2's [#7], which already states its
tier must be reconciled with the server-side design rather than inventing a parallel one. [#9] is
where the model settles, and [#10] cannot finalise its keying ahead of it. The document is
provisional, and its own §14 names delta-native serving as the feasibility item.

Two rules shape the client half. It selects **requests** and never filters **responses** — a
client trimming marks to fit a budget would be deciding what is visible, which is the server's
job and an invariant. And a client cache keyed loosely is a disclosure rather than a staleness
bug: a response served under a different credential or a superseded version coordinate shows a
viewer someone else's authorisation.

On the server side, eviction must never be a correctness event: an entry's key must include
everything whose change would make it wrong. One known gap is recorded — the row-projection bound
does not bound process memory, because every admitted request holds its projection for the
request's duration whether or not the map still does.

[#46] is the loosest item in the release. The viewer already renders masked viewports at 10⁹
against the real server, so the epic is largely a matter of documenting which display obligation
each part of it demonstrates. It can run at any point.

### Python SDK

> [#47 Provide Python and Rust SDKs for the remote service][#47]

A caller outside a browser authorises, queries a viewport, drills down and ingests, without
hand-writing the wire format. The trust boundary is unchanged: this is ergonomics, connection
handling and typed results. Python is a first-class consumer of this system and never a component
of it, and an SDK fits that. In-process embedding is a different thing and sits in phase 2, as
[#51].

The SDK is checked against the contract rather than mirrored from it, so a contract change
surfaces as a test failure. The Python already in the repository is the test-only reference
oracle and must not become the SDK — its value is precisely in not sharing code with the engine.

Publishing this and [#10] is what closes the `api_version = 1` window the slices epic depends on.

### Deployable from the documentation alone

> [#50 Let someone deploy and use this from the documentation alone][#50]

A stranger installs it, loads data, points a client at it, and gets a correctly masked map,
asking nobody. Installation, packaging, a first-run tutorial, the configuration reference and an
operations guide.

**Benchmarking finishes before this starts.** An operations guide states what to watch and what
the numbers should look like, and a configuration reference has to explain what each performance
knob costs. Written against a bracketing pair rather than the operating point a deployment
actually runs at, it documents a system nobody has. [#45] is in this release partly for that
reason: the harness exists and is good, but nothing runs it, and three viewport benchmarks
already doubled at a re-calibration without anyone noticing until afterwards.

The security model has to be stated as deployer obligations rather than as system properties:
which decisions are theirs, what they must configure, and what the system deliberately will not
choose for them. The configuration already enforces that distinction — performance settings have
defaults and disclosure controls do not, so a config file doubles as a disclosure-review
checklist — and the documentation makes it legible instead of leaving it to be discovered at
startup.

Last in the release by construction: it documents what everything else produces.

### The one that belongs to no theme

> [#56 Measure whether a precomputed tile table beats the range search][#56]

Whether a precomputed tile table beats the range search is unmeasured, and the specification
carries the table as an unbuilt artifact until someone finds out. `tile_ranges` is 26–64% of a
sparse viewport, so the cost being avoided is real — but that is the cost of the search, not the
saving from replacing it, and two optimisations already attack the same problem. It blocks
nothing, depends on nothing, and the specification changes either way it lands. The cheapest
closable item on the board.

---

## 3. Phase 2

**[#7 Serve standard map clients by tile address][#7].** MapLibre, OpenLayers and QGIS by the
addressing they already speak. The addressing is an alias over the existing viewport contract
rather than a second query surface: a second surface would need its own leak-register row and its
own conformance coverage, and the register is exhaustive only because the surface is small. Its
cache is S5 of [#9]'s model, not a parallel one, and its design
([`design/tile-addressed-integration.md`](design/tile-addressed-integration.md)) is provisional.

**[#5 Spread the index across machines][#5].** A shard is where bytes live; a partition is what a
token may reach. They are separate epics because conflating them is how a placement change
silently becomes an authorisation change. Term distribution is part of it and is constrained:
per-shard term namespaces were rejected because they dissolve the byte-equality of canonical term
descriptors that the agreement between the two authorisation functions relies on. The scaling
analysis is analysis, not specification, and says so — this is a decision for 10¹⁰.

**[#12 Divide a corpus into security compartments a token may or may not reach][#12].** A token
carries the compartments it may reach, computed once at authorisation. A compartment the token
cannot reach contributes *nothing satisfied*; a compartment the system cannot reach is an error,
and collapsing the second into the first is fail-open. I13b and I13c are both specified and
unimplemented. One hardcoded partition today, which is why the gap is currently harmless — and
why [#41]'s containment gets built single-partition first.

**[#44 Filter by text and by vector similarity][#44].** Separated from [#43] because the
machinery, the risk and the available prior art all differ. A vocabulary or term-frequency
response is an aggregate over the whole corpus including items the viewer cannot see, which the
system's own rule classifies as a disclosure rather than a filtered view. The vector half has no
design at all — a ruling before it is work — and its hard part is that an ANN index is a
precomputed structure over all items, which the sampling invariant permits only as a fast path
with an exact fallback.

**[#51 Embed the engine in the caller's process][#51].** Promoted off the wishlist, and it opens
with a ruling rather than with work. Several guarantees are statements about a boundary that
would no longer exist: entity identifiers never crossing the trust boundary is meaningless where
there is no boundary, and the client-facing identifier is a blinding permutation that explicitly
does not defend against anyone holding the bundle — which an in-process caller does. The honest
answer is probably a smaller set of guarantees stated plainly rather than the same ones asserted.
Distinct from [#47], where the boundary is unchanged.

---

## 4. Recorded, not scheduled

**[#53 Select by arbitrary region, not only by rectangle][#53].** A drawn polygon or an
administrative boundary instead of the rectangular viewport. A set producer over the existing
index rather than a change to how points are stored: a region resolves to cells, and cells to row
ranges the machinery already handles. It joins the filter contract, so it follows [#43] whenever
it is taken up. Distinct from [#54], which changes what an item *is*.

**[#52 Back up and restore a deployment without re-exposing deleted items][#52].** The hazard
determines the design: restoring a state that predates a deletion re-exposes the deleted item. An
older consistent state is not a safe state when the thing that changed was a revocation. It
cannot be a directory copy — a restored bundle reconciles against the deny record before serving,
or refuses. It should not start before [#4], because the record it must reconcile against does
not exist.

**[#54 Store and serve items that are areas rather than points][#54].** This reaches the central
assumption of the storage model. Geometry is stored in Morton order so a tile is a contiguous
range of rows, and that single property is what makes a masked count bitmap arithmetic instead of
a scan. It holds because each item occupies one cell; a polygon spans many, at several depths,
and has no single position. Every downstream property rests on it. No design exists.

[#3]: https://github.com/jennis0/tessera-index/issues/3
[#4]: https://github.com/jennis0/tessera-index/issues/4
[#5]: https://github.com/jennis0/tessera-index/issues/5
[#6]: https://github.com/jennis0/tessera-index/issues/6
[#7]: https://github.com/jennis0/tessera-index/issues/7
[#8]: https://github.com/jennis0/tessera-index/issues/8
[#9]: https://github.com/jennis0/tessera-index/issues/9
[#10]: https://github.com/jennis0/tessera-index/issues/10
[#11]: https://github.com/jennis0/tessera-index/issues/11
[#12]: https://github.com/jennis0/tessera-index/issues/12
[#13]: https://github.com/jennis0/tessera-index/issues/13
[#14]: https://github.com/jennis0/tessera-index/issues/14
[#41]: https://github.com/jennis0/tessera-index/issues/41
[#42]: https://github.com/jennis0/tessera-index/issues/42
[#43]: https://github.com/jennis0/tessera-index/issues/43
[#44]: https://github.com/jennis0/tessera-index/issues/44
[#45]: https://github.com/jennis0/tessera-index/issues/45
[#46]: https://github.com/jennis0/tessera-index/issues/46
[#47]: https://github.com/jennis0/tessera-index/issues/47
[#48]: https://github.com/jennis0/tessera-index/issues/48
[#49]: https://github.com/jennis0/tessera-index/issues/49
[#50]: https://github.com/jennis0/tessera-index/issues/50
[#51]: https://github.com/jennis0/tessera-index/issues/51
[#52]: https://github.com/jennis0/tessera-index/issues/52
[#53]: https://github.com/jennis0/tessera-index/issues/53
[#54]: https://github.com/jennis0/tessera-index/issues/54
[#55]: https://github.com/jennis0/tessera-index/issues/55
[#56]: https://github.com/jennis0/tessera-index/issues/56
