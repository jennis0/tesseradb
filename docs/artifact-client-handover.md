# Handover — putting Stage 2's artifacts on the map

**Date:** 2026-08-16 · **Status:** Working handover, not normative · **For:** the agent integrating
annotation artifacts into `clients/ts/`

The engine and the server can now register an annotation layer, take a clustering, and serve each
cluster with the masked count that viewer's own visible set generates. Nothing draws it. Your job is
to make the map show it, so the owner can look at two principals side by side and see the counts
differ.

The work is on branch `artifacts/stage-2` (worktree
`.claude/worktrees/artifacts-stage-2`), commits `38eb42f`, `06c7542`, `303e7b4`, branched from
`artifacts/stage-1`. Neither branch is merged to `text/owed-work`. Stage state lives in
[`artifact-delivery.md`](artifact-delivery.md) §3 — that file, not GitHub issues, is the record for
this programme (owner direction).

---

## 1. Read this first: three decoders, one of them updated

The frame grammar has **three independent implementations**, deliberately (contracts §0.2's
second-reader posture). A response carrying artifacts is a **kind-5 frame**, and all three refuse an
unknown kind rather than skipping it:

| Decoder | Path | Knows kind 5? |
|---|---|---|
| Rust | `crates/tessera-wire/src/payload.rs` | ✔ yes |
| TypeScript | `clients/ts/core/src/frame.ts` | ✘ **throws** |
| Python oracle | `reference/oracle/wire.py` | ✘ **throws** |

**So the first artifact a viewport serves breaks the browser client and the conformance oracle**, with
`unknown frame kind 5`. Neither is covered by `cargo test`, which is why the Rust gate is green and
this is still true. Teaching the TS decoder the frame is your first task; the Python oracle is not
strictly yours, but leaving it is leaving a landmine for whoever runs the suite next, and it is a
handful of lines. Refusing an unknown kind is correct and must stay — do not soften it to a skip.

The captured goldens (`clients/ts/core/test/fixtures/viewport-*.bin`) were taken against a bundle
with no layers, so they carry no kind-5 frame and still pass. Re-capture only if you want a golden
that exercises artifacts, and if you do, re-capture deliberately: a decoder test passing against a
stale golden is worse than no test.

---

## 2. What exists to call

### Registering a layer — `PUT /control/layers`

Operator credential. Returns `201` and `{"name": ..., "tessera_id": "..."}` — that identifier is the
only address by which the layer can later be suppressed.

```json
{
  "name": "clusters/hdbscan-2026-08",
  "title": "HDBSCAN clusters",
  "slices": ["s0"],
  "membership": "enumerated",
  "access": { "label": null, "artifacts_carry_own": false },
  "visible_when": { "min_visible": 25 },
  "hierarchy": { "kind": "flat", "prune_children": false }
}
```

`content`, `depends_on` and `levels` default; everything else is required and unknown fields are
refused. `slices` must name a slice the bundle actually carries — read it from `/v1/meta`, don't
assume `"s0"`.

- `access.label` gates the layer: a principal whose terms do not satisfy it is told the layer does
  not exist, by exactly the route a never-registered name takes.
- **`access.artifacts_carry_own: true` serves nothing today.** Per-artifact labels arrive with
  content at Stage 3; until then the flag has nothing to satisfy and every artifact is withheld,
  fail-closed. Declare `false` for anything you want to see.
- `visible_when` is `{"min_visible": n}`, `{"min_fraction": p}` with `0 < p ≤ 1`, or `null` for no
  rule. Start with `null` while you are wiring the pipe, then turn it on — it is the control that
  makes clusters vanish for one principal and not another, which is most of what you are here to
  show.

Refusals are `422` and say why in plain terms. A dropped name is refused for ever.

### Publishing artifacts — `PUT /control/layers/{name}/artifacts`

Operator credential. **The layer name is path-shaped, so its slash is percent-encoded into the one
path segment the route captures:** `/control/layers/clusters%2Fhdbscan-2026-08/artifacts`. The drop
route already works this way.

```json
{
  "level": 0,
  "addressing": "external",
  "artifacts": [
    { "stable_key": "c-0001", "members": ["<base64 external id>", "..."] },
    { "stable_key": "c-0002", "members": ["..."] }
  ]
}
```

- `addressing` is `"external"` or `"tessera"`, **per request, not per member** — a clustering names
  its whole corpus, and a per-member tag would be most of the body.
- External ids are **base64**, the same encoding `/control/changes` takes; they are bytes, not text.
  The bench fixtures' convention is the source corpus's numeric id as 8 bytes little-endian (see
  `external_id_of` in the server's test fixtures).
- `"tessera"` addressing additionally requires `"idset"`, from `/v1/meta`. It is refused beside
  external ids.
- Response is `201` with one `{"stable_key", "tessera_id"}` per artifact, in submitted order.

Three refusals you will meet:

- **An unresolvable member refuses the whole batch** (`404`), naming `member N of artifact M`. It is
  deliberate: a silently dropped member shrinks both the count a viewer is shown and the size the
  proportional criterion divides by, so a typo would move clusters across their own threshold in the
  direction of hiding them.
- **A repeated `stable_key` is refused** (`422`). Publication is append-only; an edit is a delete
  plus a re-publish, and the delete half is Stage 7's. To re-run a clustering during development,
  drop the layer and register a new name — names are never reused, so pick `…-v2`.
- **A member that is not a point is refused.** Members are documents. Another artifact or a layer
  entity would count towards the declared size while being visible to nobody.

Batches are the commit unit; publish a clustering in a few large batches rather than one call per
cluster (one fsync each).

### Reading it back — `GET /v1/meta`

Already carries a **per-principal** `layers` array: `name`, `title`, `slices`, `membership`,
`hierarchy{kind, prune_children}`, `levels`, `derived_content`, `supplied_content`, `depends_on`,
`version`. It never carries the artifact count and never the gate label. This is the right place for
the client to learn which layers to offer as toggles.

`hierarchy.prune_children` is the layer's **default** cut depth, not its only setting.

### Reading it back — `POST /v1/viewport`

Two new optional request fields:

```json
{ "slice": "s0", "zoom": 4, "bbox": [...], "k": 200,
  "layers": ["clusters/hdbscan-2026-08"],
  "artifact_budget": 500 }
```

- **`layers`** — absent means every layer this principal reaches; `[]` means none and costs nothing.
  It narrows and never widens: naming a layer you cannot reach is not a way to learn it exists. Use
  it for the layer toggle, so a client rendering one layer does not pay for the others.
- **`artifact_budget`** — accepted, and **inert on a flat layer**, which is everything Stage 2 can
  publish. Artifacts are never sampled to meet a budget: dropping half the clusters gives a wrong
  map rather than half a map. The field exists now because it is a wire shape and adding a request
  field to a shipped frame later is what this ordering avoids. Do not build UI that assumes it
  truncates.

### The response frame

Kind 5, Arrow IPC, one row per served artifact, delivered **after the tiles frame and before any
points frame**:

| column | type | |
|---|---|---|
| `layer` | utf8, non-null | the layer's name |
| `tessera_id` | uint64, non-null | the artifact's opaque identifier |
| `stable_key` | utf8, **nullable** | the publisher's own key, if they supplied one |
| `masked_count` | uint64, non-null | **how many members this principal can see** |

**The frame is absent when nothing is served** — same rule as the points frame. A deployment with no
layers pays nothing. Absent and empty carry the same information: `/v1/meta` already tells the
client which layers it reaches.

`tessera_id` is `uint64`, matching the points frame's, so one decoder path handles both. As
elsewhere on this wire, treat it as an opaque token — do not sort by it, do not derive from it, and
prefer a string when it crosses into JSON, because a bare JSON number loses a `u64` past 2⁵³.

---

## 3. What the numbers mean, and what will look like a bug

**`masked_count` is the count over the whole cluster, not over the viewport.** It does not change as
the user pans or zooms. Only *whether* the cluster appears depends on the viewport. This is
deliberate: a per-viewport count would let a viewer difference two boxes and recover the members in
between. If you build a panel that says "N documents", it will hold steady while the map moves —
that is correct, and worth a tooltip.

**A cluster appears when any member visible to this principal falls inside the requested tiles.**
There is no bounding box anywhere in the design; a box over full membership would disclose a
cluster's true extent by panning.

**A cluster below its criterion is absent, with no reason given.** The response carries nothing that
distinguishes it from a cluster that was never published, from one whose layer this principal cannot
reach, and from one that was suppressed. Do not invent a "hidden" state in the UI — there is nothing
to populate it from, by design.

**Two principals see the same `tessera_id` for the same cluster.** Identity is stable across
sessions and principals; only the number beside it moves. That is the intended trade (Appendix C's
C17), and it is what lets you put two principals' answers side by side and compare.

**Never put these on screen or on a wire you author:** the cluster's ordinal, its unmasked
membership size, or its member list. The ordinal is a position in a dense level, so two of them
count what lies between; the size is a corpus-wide count over items the principal may not see. The
server does not send them and it should stay that way.

---

## 4. Gaps you will hit

**There is no HTTP route for artifact drill-down.** `Engine::artifact(session, tessera_id, idset,
slice)` exists and is tested — it returns the same `ArtifactOut` the viewport does, from the same
predicate — but nothing exposes it. `POST /v1/items/{tessera_id}` handles **points only** and will
answer `404` for an artifact identifier, because an artifact has no term postings for the
entity-space visibility test to find.

If the client needs click-a-cluster, adding the route is small and the engine method is ready.
Two things it must preserve: the caller supplies the slice (a masked count is per row space), and
the answer must come from `Engine::artifact` and nothing else — a cluster reachable by identifier
but not by viewport would be the one rule transcribed twice, which is the failure this codebase
keeps writing down.

**`min_visible_members` in `dev-server.toml` is parsed and unused.** The criterion is per-layer
(`visible_when`) and the deployment-wide key predates it. Reconciling the two is open work on the
tracker; for now, set the behaviour you want in the declaration and ignore the config key.

**The write-ahead log grows from the first publication and is never trimmed.** Membership has no
home on disk outside the log yet — that packaging decision is the owner's, and unbuilt — so
reclamation is pinned fail-closed. For a dev loop this is fine; if you publish a large clustering
repeatedly against a long-lived server, watch the log's size rather than being surprised by it.

**Layers and their artifacts live in the WAL, not the bundle.** They survive a restart of the same
server against the same WAL, and they do **not** travel with the bundle directory. Point a fresh WAL
at the same bundle and the layers are gone. Script the register-and-publish step; do not treat it as
one-time setup.

---

## 5. Suggested route to something the owner can look at

1. Teach `clients/ts/core/src/frame.ts` kind 5 and decode it in `decode.ts` beside `subCells`. Keep
   the strictness — unknown kinds throw.
2. Get a bundle and a server running per [`clients/ts/README.md`](../clients/ts/README.md). Note
   `data/bench-fixtures/` is not in the repo; build one, or use whatever the owner has locally.
3. Write a script beside `clients/ts/scripts/` that registers a layer and publishes a synthetic
   clustering over the fixture's own external ids — spatial k-means over the points is plenty, the
   point is the masking, not the clustering. Make it idempotent by versioning the layer name.
4. Draw the clusters. A hull or a labelled centroid is the obvious thing; there is no geometry on the
   wire yet (derived content is Stage 3), so you are drawing from the member positions the points
   frame already carries, or a marker per cluster placed client-side.
5. Show the count beside each cluster, and make the principal switcher (`presets.json`, already
   built by `measure-principals.mjs`) flip between two principals over the same view.

**The screenshot that proves the stage** is two principals, one clustering, visibly different counts
on the same cluster — and at least one cluster present for one and absent for the other. That second
half needs `visible_when` set to a bar the narrow principal misses; pick it from the numbers the
broad principal reports.

---

## 6. Where the code is

| | |
|---|---|
| The one predicate | `crates/tessera-engine/src/artifacts.rs` — `ArtifactView::verdict` |
| Membership, durable form | `crates/tessera-lifecycle/src/membership.rs` |
| Layer registry, publication | `crates/tessera-lifecycle/src/registry.rs` |
| Viewport serving pass | `crates/tessera-engine/src/viewport.rs` — `Engine::serve_artifacts` |
| Drill-down (unrouted) | `crates/tessera-engine/src/viewport.rs` — `Engine::artifact` |
| Control verbs | `crates/tessera-server/src/control.rs` — `register_layer`, `publish_artifacts` |
| Wire frame | `crates/tessera-wire/src/payload.rs` — `artifacts_frame` |
| What the counts must do | `crates/tessera-engine/tests/artifact_serving.rs` |
| What the wire must not carry | `crates/tessera-server/tests/layers.rs` |

The design is [`annotation-representation.md`](design/annotation-representation.md) (normative) and
[`annotations.md`](design/annotations.md); the rulings that shaped Stage 2 are decisions 0074, 0075,
0079, 0080 and 0083. Read the tests before the design if you are short of time — they carry the
failure modes in their names.
