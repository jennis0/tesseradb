# Implementation plan — the configuration surface

**Date:** 2026-08-18 · **Status:** Plan — evidence, not normative. The design is
[`2026-08-18-configuration-surface.md`](2026-08-18-configuration-surface.md), ruled by
[decision 0088](../../decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md) and
binding through [`configuration.md`](../../design/configuration.md) — a new normative document,
extracted from `per-point-attributes.md` §4 — and
[`annotation-write-cycle.md`](../../design/annotation-write-cycle.md) §6.1, both already written.

## What this is, and what it is not

Nine stages, ordered by what forces the order rather than by size. **Every stage ends green on the
full gate** — `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`scripts/check-layers.sh`, `scripts/check-doc-links.py` — and every stage is a commit. Nothing here
is a migration: pre-release, a format changes and the artifacts are recreated
([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)). Do
not add an alias, a `#[serde(default)]` or a compatibility shim at any point in this plan; a
retired key must be refused by `deny_unknown_fields`, which is what tells a caller their file is
stale instead of silently reading it wrong.

**Measured blast radius**, so no stage is a surprise: 22 Rust files write a `schema.toml` or a
`layers.toml`; `slice` appears 1,719 times across 177 Rust files and 347 times in `docs/design/`.
The rename is the largest stage by line count and the smallest by risk, which is why it is last.

## The stages

### 1 — `slice` → `view`, mechanical *(no behaviour change)*

Its own commit, before the parsers, so no later diff is half-rename. `SliceDescriptor`,
`slice_id`, `--slice`, the per-slice layout path segment, `slices-and-multi-table.md` and every
citation of it. **Sequencing constraint:** another session holds uncommitted changes in
`crates/tessera-build/src/layers.rs` and the engine's cut; this stage touches both, so it waits for
those to land or is done in a worktree rebased onto them.

⚠ **Confirm before executing.** The memo records the one concern: *viewer* is load-bearing here —
the principal is a viewer, visibility resolves per viewer, `/v1/meta` is per-viewer — so `the
view's visibility` and `the viewer's visibility` differ by one word in a corpus that makes both
claims constantly. `projection` and `space` carry the same generality without it. The ruling
stands; this is the last cheap moment to reverse it.

### 2 — The config file: one parser, two axes

**The target is `configuration.md` §1's table, exactly.** It enumerates the whole surface —
six blocks, every key, every enumerated value word — and the set is closed: `deny_unknown_fields` on
every block, an enumerated set behind every word. Two consequences for this stage. A parser that
accepts a key the table does not name has widened the surface silently, so **the table is the test**:
assert the accepted key set per block against it, and the assertion fails when someone adds a key
without reasoning about it. And every retired key is refused by the unknown-field rule rather than
aliased, which is what tells a caller their file is stale.


`schema.rs` and `layers.rs` collapse into one module reading one document. Deliver in this order,
because each step's refusals depend on the last:

1. **`[[vocabulary]]` as an object** — `name`, `title`, `width`, `value_set`, `visibility`,
   `source`/inline values. Retire `values_key`, `values_of` and `vocabulary = "declared"|"discovered"`.
   New refusals: an attribute naming an undeclared vocabulary (**at config parse, before any data
   file opens**), two blocks of one name, `value_set = "closed"` with no value source. Codes become
   optional — a bare key list assigns them — which brings the carry rule with it: assigned codes
   are recorded in the manifest and replayed on rebuild, a live code never changes, and a removed
   value's code goes to `reserved`. Width moves here,
   which makes cross-attribute width disagreement inexpressible rather than refused.
2. **`visibility` everywhere** — replacing `listing`, `gate`/`ungated`. Closed value set per site:
   a layer takes a label, a vocabulary takes `public` or `derived`, supplied content takes
   `derived` or `inherited`. A bare word outside the site's set is refused, never read as a label.
3. **`artifact_visibility` / `point_visibility` as `{ field, default }`**, retiring
   `artifacts_carry_own`. Presence of `field` is the own-labels declaration (C27).
4. **`require_member_visibility`**, absorbing `visible_when` and `corpus_derived` (C28). Its
   threshold words are renamed with it: `ExistenceCriterion::MinVisible` becomes `{ count = n }`
   and `MinFraction` becomes `{ fraction = p }`, the `min_` prefix being redundant once the key
   says *require*. The two keep their existing semantics, including that the proportional form
   ⊘ breaks rollup.
5. **`title` everywhere nameable**, and vocabulary values' `label` → `title`.
6. **`on_member_deletion` → `withdraw_on_member_deletion = true | false`** on `[layer.content]`,
   behaviour unchanged and `true` still the default. **New at the layer level**, defaulting
   `false`: withdrawing the whole artifact rather than only its supplied content. That is a
   behaviour change, not a rename — it needs a fold path and a test that an attached artifact's
   dangling-dependent refusal still fires — so give it its own commit.

**The property this stage must not lose**, and the one to write a test for first: every disclosure
control stays required with no serde default. Parse each as `Option<T>` and hand-validate, so the
message teaches — *what is missing · the values, spelled out · what each does · why there is no
default* — rather than emitting raw serde text. Two existing messages are wrapped without a
continuation and print twenty-two spaces mid-sentence; fix both here.

### 3 — Sources, fields and the invocation

*Built, and not as written here.* `source` was drafted as a bare key bound by `--file KEY=PATH`,
which replaced five flags with five more and produced a nine-flag command line beside a detailed
config. What shipped instead: **`source` is a path relative to the declaring document**, an
absolute one refused; `--file` survives as an **override keyed by the object** whose source it
replaces; the **extent moved into `[[view]]`** and `--extent` is deleted; a **`tessera.toml`**
found by walking up says where the declaration is and where the bundle goes; and the identity key
comes from the environment, `--id-key` deleted with it. `tessera build` and `tessera serve` are
the whole invocation. The three fail-closed rules survive in substance — a source with no path from
anywhere, an override no object declares, and **never a fall-through to minting** — and field maps
still say *where*, never *whether*. `entity_id` is the canonical identity field, declared once on
`[corpus]`. See `configuration.md` §3 and §8.

### 4 — Input readers: named fields, list access terms

Remove the hardcoded `entity_id`/`x`/`y`/`term_id` lookups in `input.rs` in favour of the resolved
field map. Add the access relation as a `list<string>` or plain `string` field, minted as an open
vocabulary. Rules from the fail-closed review, each with a test: null and empty both mean *no
terms, visible to no principal*; terms are trimmed; `public` resolves to reserved term `0`.

**Term `0` is reserved in the dictionary and satisfied by construction.** The addition happens in
the engine when a principal's term set is resolved — inside the trust boundary, not in the plugin
and not from the credential. This is the one step in the plan that changes what a request computes;
give it its own commit and its own conformance case.

### 5 — The plugin takes a term list

A trait entry point accepting `Vec<Descriptor>` beside the existing byte-label one; passthrough
implements it as the identity. Removes the comma join, and with it the widening where a caller's
term containing a comma split into two grants. Bump the plugin hash — it is recorded in
`MANIFEST.json` precisely so a bundle cannot be served by a plugin that would label items
differently.

### 6 — Artifact and member sources

One row per artifact with `contents` as a ranked list, retiring the cross-row agreement refusal;
`variation` → `rank`, `member` → `entity`. One source per layer, so the `layer` discriminator
column goes. Membership by exclusion as an input spelling — complement once at build, materialise
the same set, and **no request-time complement**. Inline `artifacts = [...]` for authored layers.
`stable_key` → `key` throughout, including `IncomingArtifact` and the registry, and
`children_keys` is deleted — it is read, validated and never walked.

### 7 — `membership` as a table, and `[layer.labels]`

`membership = { attribute = "…" }` so attribute membership can name its field. Then the labels
sugar, expanding to a real layer: same views, flat, `depends_on` the parent, content wrapper
supplied; gate, membership requirement and membership data all written out by the caller. The
label layer's `visibility` defaults to its parent's — the one defaulted disclosure control in the
surface, admissible because it is the parent's value and never the widest.

### 8 — `tessera check`, `--extent auto`, and the disclosure report

**`tessera check` should also emit control-plane payloads** (`configuration.md` §2). A deployment
that declares but never builds authors the same layer twice — once as TOML to compile an empty
bundle, once as JSON to create it online — and the parser has already done the work by the time it
could emit the second. Cheap here, and it closes the one ⊘ the split leaves open.


- **`tessera check`** parses the config and reads only Parquet *schemas*: every declared attribute
  against the field that must carry it, every source bound, every layer's view declared, every
  disclosure decision as a table. Seconds, and it is what goes in CI.
- **`extent = "auto"`** — *landed at stage 3, as a `[[view]]` key rather than a flag* — plus — mattering more — **a report of how many points clamped to an extent
  edge, printed with the data's actual bounds beside the extent given**. The notebook shipped a
  degenerate map for exactly this reason and the build said nothing. Without the clamp report,
  `auto` only moves the trap.
- **`reports/disclosure.json`** beside `containment.json`: every layer with its `visibility` and
  `require_member_visibility`, every vocabulary with its `visibility` and `value_set`, every
  attribute with its placement, and what `[layer.labels]` expanded to. Diffable between builds.

### 9 — The notebook, and the corpus tail

`notebooks/arxiv-corpus.ipynb` emits one config and one source per layer; the term dictionary
becomes a real build output rather than the notebook's `terms.parquet`. Then the citations: the
22 Rust files writing the old TOML, `records-and-search.md` §2, `annotations.md`, and
`docs/artifact-delivery.md`'s status lines.

## What to watch

- **The register is shorter, not weaker.** C27 and C28 now follow one key each under new names
  (architecture r46). If a stage leaves either without an explicit, undefaulted declaration, the
  stage is wrong — that is the whole of what the register watches.
- **`--carry-id-key-from` must carry the term dictionary** alongside the key once terms are minted
  from caller strings. Remembering one and forgetting the other silently renumbers entity space,
  and `tessera_id` derives from it.
- **Two things in the design are settled by argument rather than by ruling** and may be revisited
  without disturbing the rest: the label layer's inherited gate, and `membership` as a table.
- **Stage 1 and stage 6 both touch `layers.rs`**, which another session is editing. Sequence around
  it rather than merging over it.

## Suggested commits

One per stage, in order, each green on the gate. Stages 2–3 are the ones worth a review pass before
the rest lands: they set every refusal the later stages assume.
