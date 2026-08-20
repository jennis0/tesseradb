# Handover — for work continuing on artifacts

**Date:** 2026-08-20 · **Status:** Complete. Branch `artifacts/stage-4`, `c3a595a`..`60b7f47`.

You were paused while the configuration surface was reworked. That finished, and a second body of
work landed on top of it: **artifacts can now be declared by the points that belong to them**, at
both entry points. Between them they moved most of what you are about to touch — the layer
declaration, the artifact input shape, three field names inside `IncomingArtifact`, what a
`depends_on` edge *means*, and what an ingest batch may carry.

Read this before rebasing anything. **`docs/design/configuration.md` is normative for the surface**
and `docs/design/artifacts-from-points.md` for the second body of work; both win over this document
wherever they differ. This is a map of what moved, not a second specification.

**Two rulings govern everything below**, and they are worth reading before the detail:

- [Decision 0089](decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md) — a
  dependency edge carries deletion and visibility. §1.
- [Decision 0091](decisions/0091-build-is-ingest-into-an-empty-database.md) — **there is no
  difference in functionality or client experience between a build and an ingest.** A build is a
  more efficient form of ingesting into an empty database; internals may differ, what a caller can
  *say* may not. A feature that works at one entry point and not the other is unfinished rather
  than staged, and a build-only refusal is a bug unless it is about where rows come from.

## 1. The change that alters artifact semantics

**[Decision 0089](decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md): a dependency
edge carries deletion and visibility.** `depends_on` used to declare an edge and say nothing about
what the edge did. It now carries both of the rules the member grain already had:

1. A dependent artifact is **deleted when the artifact it depends on is deleted**.
2. A dependent artifact is **visible only where the artifact it depends on is visible** — a
   prerequisite evaluated *before*, and in addition to, its own `visibility` and
   `require_member_visibility`.

Neither is configurable, deliberately: four knobs across two grains is a surface no author can
hold, and the pair chosen is fail-closed in both directions.

**The prerequisite is per-artifact, not per-layer.** A label is gated on the artifact it attaches
to, not on the parent layer having anything visible. It lives in `ArtifactView::verdict` as a third
branch of the one predicate every serving route calls — not beside it — so the viewport and the
identifier route reach it by the same call. A dependent whose target this viewer cannot see is
**absent, not empty**: it contributes to no count in the response. Chains recurse through
`Engine::dependency_served`, with a fail-closed depth backstop rather than unbounded recursion on a
request path.

**This reversed [decision 0086](decisions/0086-the-attachment-term-does-not-inherit-the-targets-criterion.md),**
which had declined exactly this inheritance and named the asymmetry it was accepting: a viewer too
sparse to be shown a cluster would still be shown the label written about it. If you find 0086 while
working, it stands unedited by convention — 0089 records the reversal and answers both of 0086's
arguments. **The cost 0086 priced is now being paid**: a second masked count per attached artifact
per request.

**A refusal you will hit.** An artifact in a layer that declares `depends_on` must itself declare a
dependency, into the declared layer. Refused at build *and* at ingest — an ingest that admits what a
build refuses is the fail-open half.

**Not affected:** an artifact withdrawn by `withdraw_on_member_deletion` is not deleted, so it does
not cascade — but its dependents are absent anyway, because a withdrawn artifact is not served and
rule 2 reads that directly. Nothing is stranded, and it needed no special case.

## 2. The artifact input shape

One long table with a `layer` discriminator column, content duplicated across rows, and a cross-row
agreement refusal is **gone**. What replaced it:

```toml
[[layer]]
name       = "clusters/hdbscan"
title      = "HDBSCAN clusters"                  # optional
views      = ["s0"]
source     = "hdbscan"                           # a `[sources]` name — see §6
fields     = { members = "members", parent = "parent_id" }
membership = "enumerated"                        # or "spatial", or { attribute = "<field>" }
hierarchy  = { kind = "nested", prune_children = true }

visibility                = "public"             # an access label, or `public`
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }  # all | any | {fraction} | {count} | none
content                   = { computed = ["centroid", "box"] }

  [layer.members]                                # membership in its own source, one row per
  source = "hdbscan_members"                     # (artifact, entity); or on the artifact row

  [layer.labels]                                 # sugar — see §4
  ...
```

- **One source per layer.** The `layer` discriminator column does not exist; there are no other
  layers' rows in the file to tell apart.
- **One row per artifact**, with `contents` a ranked list — entry *k* is `contents[k]`'s values.
  The two cross-row agreement refusals (attachment, parent) are deleted because an artifact is one
  row and there are no copies to disagree. **A key on two rows is refused** as two artifacts under
  one name; that is the agreement refusal's successor, not its survival.
- **`artifacts = [...]` inline** replaces `source` for an authored layer. Both is refused; neither
  is legal (a declared, empty layer is the normal write-path state — see §6).
- **Membership may be spelled by exclusion.** `excluding` beside `members`, for a set that is nearly
  the whole corpus. **The complement happens once, in the build**, against the entity space the build
  assigned. There is **no request-time complement and none is expressible** — `excluding` exists only
  inside the build's plan type, and nothing below `resolve_artifact` can represent a complement. A
  complement evaluated against a viewer's mask would tell that viewer about items outside it. An
  excluded id the build did not assign refuses the build: an exclusion that resolves to nothing
  silently *widens*.
- The three spellings are input spellings and nothing more, pinned by three tests asserting a
  **byte-identical bundle**: inline against sourced, `excluding` against its inclusion, and a row
  membership against `[layer.members]`.

**Known scaling hazard, not a bug:** the complement materialises. An `excluding` list of three ids
over a 10⁸-point corpus produces a membership vector of ~10⁸ entity ids. Correct per the design,
and unresolved.

## 3. Identifier renames — these will break a stale patch

Workspace-wide, ~620 sites. If you have work in progress, this is the part that will not apply.

| Was | Is | Note |
|---|---|---|
| `variation` | `rank` | the index into an artifact's ranked contents |
| `variations` | `contents` | the list itself |
| `IncomingVariation`, `PublishedVariation`, `VariationSet`, `PlannedVariation` | `IncomingContent`, `PublishedContent`, `ContentSet`, `PlannedContent` | the thing *at* a rank cannot also be a rank |
| `member` | `entity` | the membership row's **scalar** field only |
| `stable_key` | `key` | including `IncomingArtifact` and the registry |
| `children_keys` | *(deleted)* | children are derived by inverting parent edges |

`members` (the list), `member_count`, `require_member_visibility` and `withdraw_on_member_deletion`
all stay. **`rank` collides with three established meanings** — Morton rank, Zipf rank, rank within
a bitmap — so where a renamed identifier sits near those, the comment says which rank it is. Do not
"fix" the collision by renaming; `artifact.rank` is unambiguous in its own context and the name is
settled.

Separately, retired *configuration* words that survived as identifiers are gone:
`is_corpus_derived()` → `requires_all_members()`, `ArtifactVisibility::carry_own` →
`carries_own_labels`, and `min_visible_members` is `require_member_visibility` or "the existence
criterion" everywhere it was still written as a key.

## 4. `[layer.labels]` is real, and is sugar only

It used to be refused whole. It now expands into a `[[layer]]` **before anything compiles**, so
there is no label layer for the compiler to treat specially — it meets every refusal, allocator rule
and reader a hand-written layer meets, and a test asserts the sugar and the layer written out build
a **byte-identical bundle**.

**The sugar supplies mechanism and not one disclosure control.** It fills in the parent's `views`, a
flat `hierarchy`, `depends_on = [parent]` and the content wrapper. Every control is the caller's:
both member requirements and `artifact_visibility` are required keys. `visibility` is the single
defaulted control in the surface, and only because the value it takes is the parent's own.

**Two requirements, two grains, and they are not one dial.** The layer's
`require_member_visibility` is a *threshold* — how much of the set a viewer must see for the label
to appear — and takes `{fraction}`/`{count}`. `[layer.labels.content]`'s is a *provenance
declaration* and takes exactly two words: `all` (a synthesis of the members, read only where every
document behind it can be) or `inherited` (true whether or not any of them exists — a name a person
wrote, adding no requirement beyond the artifact's gate).

⊘ **"Narrower, never wider" is not enforceable and there is no check pretending it is.** Whether
every principal holding one label also holds another is a fact about grants, which are not in the
declaration. The one computable case was `public`, and **that refusal was deleted by 0089**: a
`public` label layer under a gated parent now discloses nothing, because the viewer that cannot
reach the cluster cannot reach its labels. The check became a property.

## 5. Artifacts declared by the points that belong to them

`docs/design/artifacts-from-points.md` is the specification; it is **built** at both entry points.
A clusterer emits a label per point, and the clusters exist only because points reference them. The
whole design is that a cluster exists because points say it does, and everything else about it is
optional enrichment.

- **Membership from a point column needed no new surface.** `[layer.members]` already means one row
  per `(artifact, entity)`, and a point table with a cluster column is that shape:
  `source = "points"`, `fields = { key = "cluster_id", entity = "id" }`. Pinned byte-identical
  against a conventional member table.
- **A key may be an integer**, converted once for the roster and never per point. `null` and
  exactly `-1` mean *this point is in no artifact* — the row is skipped and counted, not refused.
  That one matters in practice: a condensed tree sheds a fifth to a quarter of a parent's points as
  noise at each split, and the natural input shape used to fail the build on exactly that fraction.
- **`value_set = "open" | "closed"` on `[[layer]]`**, default `closed`, decides whether a key no
  artifact declares creates one. Under `open`, `artifacts` is enrichment rather than a roster: a
  cluster the points name and the table omits exists without a title, a cluster the table carries
  and no point names is an artifact with no members, and neither is an error.
- **A list column is a hierarchy**, and the kind the layer already declares says how to read it —
  `flat` plain multi-membership, `stacked`/`tiered` one entry per level, `nested` a lineage whose
  adjacency declares parent edges. A shape disagreeing with the declared kind is refused; so is a
  child named under two different parents, because there is no correct output.
- **A membership can grow**, by a delta record rather than a restated membership. **The part that
  fails silently**: a level is packed only above its published high-water, so a grown record below
  that mark stays durable in the log and comes back after a restart *without* the point. A second
  pin holds the log member and is released only by the fold's whole rewrite. If you touch packing
  or rotation, `a_rotation_may_not_reclaim_the_member_holding_a_growth` is the test that notices.
- **An ingest batch may carry a column named for a declared layer** — the layer's own name, as an
  attribute column is named for the attribute's name and not its `field`. Its value is a key or a
  list of keys, on exactly the rules the build reads. The *meaning* of a list lives in
  `tessera-types` so the two entry points cannot drift; only the decode differs.
- **Minting happens at the window close**, not at admission, because a publish command executes in
  the bounded work lane without closing the open window and reads the same level cursor. Admission
  keeps only what can refuse one caller's batch alone.

**The rulings you must not undo** (`artifacts-from-points.md` §5): a key is a name and identity is
what minting allocates, so a deleted key that returns is a *new* artifact; at most one live artifact
per key per level; a suppressed artifact still exists, so a point naming its key joins it and it
stays suppressed; and deleting an artifact deletes its suppressions, riding the same ack and the
same fold rather than a separate sweep.

**The one fail-open the design names is unreachable here, and know why before you refactor.**
Written the natural way — *is this key unknown?* — against what is currently served, a suppressed
artifact reads as absent and a second unsuppressed artifact appears under its key. Three things
independently prevent it, and the innermost is that minting *is* a publication, and a publication
refuses a key its level already holds by consulting the store rather than the served view. Breaking
either resolution alone leaves the tests green; breaking both fails on that refusal — a 422, not a
duplicate.

## 6. The rest of the surface, briefly

- **One declaration**, not `schema.toml` + `layers.toml`. `tessera build` with no flags is the whole
  invocation: `tessera.toml` (found by walking up) says where the declaration and bundle are, the
  declaration says where the sources are, the environment carries the identity key.
- **`[sources]` names every file once** — free-form caller-chosen names mapped to paths — and every
  `source` in the declaration names one of those keys, never a path. `--file NAME=PATH` binds on the
  same name, so one override moves every reader of that file; it used to bind per object, where
  missing one left that object silently reading the old file.
- **`[defaults]` replaced `[corpus]`**, carrying a `source` and an `entity_id_field` that a view or
  an attribute takes when it names neither. It deliberately reaches no vocabulary, layer, member
  source or `point_visibility`: an absent source there is itself a declaration, and filling one in
  would turn it into an acquisition nobody wrote.
- **Any file may carry an attribute**, joined by entity id, with its own `source` and
  `entity_id_field`. A row naming an entity the build did not load is **ignored and counted**, and
  coverage prints against entities covered rather than rows dropped — a legitimate superset and a
  broken join drop the same overwhelming fraction, and only that number tells them apart. Zero
  coverage warns loudly and still builds.
- **Two axes, everywhere** ([decision 0088](decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md)):
  `visibility` (which access label) and `require_member_visibility` (how much of the membership the
  viewer must already see). Disclosure controls have **no defaults** — the defaultable value is
  always the widest.
- **`public` is a reserved label interned at term `0`** in every bundle, added to every principal's
  satisfied set inside the trust boundary. Resolved by descriptor lookup, so a bundle lacking it
  adds nothing rather than granting whatever was interned first.
- **Points carry their own labels** via `point_visibility = { field = …, default = … }` on a view.
- **Declaring without building is first-class.** Acquisition keys (`source`, `fields`, inline
  `artifacts`) are absent from a write-path deployment; the declaration half is identical and
  required. An `R` on an acquisition key means *required to build from a file*, never *required to
  declare* — which is why a layer with neither `source` nor `artifacts` is legal.
- **`tessera check`** reads Parquet footers only, collects every finding, and `--payloads` emits the
  control-plane bodies so a declare-only deployment does not author every layer twice.
- **`reports/disclosure.json`** beside `containment.json`: every layer's gate, member requirement,
  `depends_on`, membership and content; diffable between builds by construction.
- The closed key set is pinned: `the_accepted_key_set_is_configuration_ms_table` reads the parser's
  accepted keys out of serde's own error message and compares them to `configuration.md`'s tables.
  **Parser and doc table move together** — a key you add is a doc change in the same commit.

## 7. The gate is six commands now

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-clients.sh
python3 scripts/check-doc-links.py
python3 scripts/check-corpus-integrity.py
```

Baseline **1816 Rust tests, 0 failing, 11 ignored**, plus **210 client tests**.

`tessera-engine --test write` has timing-sensitive deny-latency cases that can fail under load from
a concurrent cargo invocation; re-run that binary in isolation before reporting one as yours.

- **`--no-fail-fast` is not cosmetic.** Without it cargo stops at the first failing binary and skips
  the rest, so a run reporting no failures beside a *smaller* passing total reads as success. That
  has already been mistaken for a green gate here.
- **`check-clients.sh` is new** and exists because one rename shipped three defects into
  `clients/` that a Rust-and-Python gate could not see. It typechecks core, viewer and spike, runs
  both suites, and typechecks the operator `.mjs` scripts with `checkJs` — the third defect was
  TS2448, which a syntax check passes.
- The `dropping_a_client_connection_mid_viewport_releases_the_gate_promptly` flake is **fixed** and
  was never a timing flake: the request was being shed with a 429 the test discarded. Do not
  reintroduce a poll that waits on an instantaneous gauge without also watching `shed_total`.

## 8. Open items in your path

Things deliberately left, so you neither rediscover them nor assume they are done.

- ⊘ **The artifact's own-terms gate is unbuilt.** No per-artifact term is stored, and a layer
  declaring `artifact_visibility = { field = … }` withholds — fail-closed. Comments in the serving
  path still point at "Stage 3" as where it arrives; the stage pointer is stale, the gap is real.
- **`--carry-id-key-from` does not carry the term dictionary.** Term ids are assigned by first
  appearance, so a new term appearing earlier renumbers the dictionary, changes the signature sort,
  changes permanent entity ids, and changes every `tessera_id`. Needs a carried dictionary format, a
  replay rule and a refusal when a carried term's id would move. **Do not rebuild a bundle you
  intend to keep identities across.**
- ⊘ **`tessera verify` does not check plugin-hash agreement** — it is handed a bundle and no plugin.
  `Engine::open` does enforce it (`cc11333`), including treating an empty or absent manifest hash as
  a mismatch. A bundle can verify clean and still be one this process must refuse to serve.
- ⊘ **`point_visibility`'s `source` route fills no default** where the `field` route does. Not
  filling is the narrow half, so it cannot leak.
- **`level` and `attached_level` cannot be moved by a `fields` map** — §1's tables do not name them,
  so a producer must spell those columns as declared. A real gap for stacked and tiered layers.
- **Conformance reasons were corrected, no coverage row moved** (`7ab0839`). I3, I8 and I12's
  frontier half are **untested machinery**, not machinery that does not exist — the annotation
  machinery they need is built. `conformance.md` r13 carries per-row reasons and names the tests
  that would move them; **I3 containment is the strongest candidate** and is squarely artifact work:
  two principals, one published label, the one missing exactly one generating-set member served
  *nothing* rather than the artifact without its description.
- **`clients/ts/viewer/smoke.mjs`** carries a stale assurance that `/v1/categories` answers 500 for a
  `derived` column "because the predicate is ⊘ unbuilt". The predicate is built; the test tolerates
  those 500s. Wants a look, and changing what it tolerates is behaviour rather than a rename.
- **Four things `artifacts-from-points.md` §8 leaves open**, none blocking: whether a minted artifact
  losing its last member should withdraw by default (the object *was* its membership, unlike a
  curated set); how the roster appears in `reports/disclosure.json`, since a diff reading *417
  clusters, unchanged* while every identity churned would mislead in exactly the case that matters;
  the notification owed when a pipeline rerun deletes a cluster someone had suppressed, which rides
  write-path §5.8's existing obligation rather than needing machinery; and whether a per-layer bound
  on minted artifacts is worth the key.
- ⊘ **The growth command still refuses an unknown key** whatever `value_set` says. It names an
  artifact to add members to rather than a point declaring the artifact it belongs to, so it has no
  build counterpart to differ from and sits outside decision 0091's concern. Deliberate, recorded.
- **`membership = { attribute = … }` is declared and unbuilt**, and is a *different* feature from
  everything in §5: it makes membership a predicate evaluated per request, where §5 materialises at
  a build and maintains at ingest. A predicate over a `derived` vocabulary is answered by a masked
  scan, which a viewport showing 263 clusters would pay 263 times.

## 9. Where authority lives

| | |
|---|---|
| `docs/design/configuration.md` | the normative surface — the closed key set, every refusal, the worked example |
| `docs/design/artifacts-from-points.md` | artifacts declared by their points: the readers, `value_set`, lineage, growth, the wire column, minting, and §8's open items |
| `docs/design/annotation-write-cycle.md` §6.1 | artifact-side semantics; §3.4 is the timing table |
| `docs/design/annotation-representation.md` | the representation, and §5.0.4 on edges constraining write order |
| `docs/decisions/0088`, `0089`, `0090`, `0091` | the two axes; the dependency edge; a vocabulary's single axis; build is ingest |
| `docs/artifact-delivery.md` | **the status record for artifact work**, by owner direction — not GitHub issues. Move it with the work |

Two conventions worth stating because each has bitten more than once.

**`conformance.md` r10 is the model for a correction that finds built machinery recorded as
absent** — change the reason, move no coverage row, and say that a testing gap now exists where one
previously did not. *We cannot test this* and *we have not tested this* are different claims, and
only the first is an excuse.

**Refuse only where something leaks or is irreversible.** CLAUDE.md's *What the strictness is for*
is the rule, and this branch produced several refusals that had to be unpicked — a `public` label
under a gated parent, a list column on a `flat` layer, a requirement that something declare entity
space. Each looked principled and each foreclosed something a caller legitimately wanted. Outside
the disclosure surface the default is to report the numbers and let the operator decide; a build
input is recoverable, the operator is present, and the loop is fast.
