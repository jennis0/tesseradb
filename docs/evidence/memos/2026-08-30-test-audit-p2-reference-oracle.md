# Test audit P2 — the reference oracle (`reference/oracle`, `reference/tests`)

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **the
oracle and its own tests, not the engine**: nothing below is a claim that shipped behaviour is
wrong, and no defect in shipped code was found. Track P2 of Wave 2 of the test-quality campaign
([`test-audit-campaign.md`](../../test-audit-campaign.md)). `conformance.md` §4.6 is untouched — a
row moves only when a test moves with it, and no test moved.

## Results

**The oracle is independent where independence is load-bearing, and it says so where it is not.**
That is the verdict a reader came for, and it survived a module-by-module attack. The three
constructions that would make a differential circular are all absent: the geometry oracle reads the
points file the build consumed rather than `morton.u32`, and `Bundle` **refuses** to derive a code
when no source is attached rather than falling back to the stored column (`bundle.py:396`); the
filter and text oracles read the fixture's generation functions rather than `attrs/`, holding no
dictionary and no ordinals; θ's anchor is computed from the segment and the pairs-derived mask and
is never read back from a response; and the mask is derived from the flat `pairs.parquet` relation
where the engine derives it from compressed postings. `theta_cut` is written as §7.2's recurrence
where the engine evaluates the closed form, deliberately, so the agreement is not copy-paste.

**Two echoes exist and both are declared at the site.** `viewport.Selection` sorts by the stored
`tessera_id` column, which is also what the engine sorts by — stated in the module doc, which
withdraws an earlier draft's claim that no artefact is shared. And `text.py` reaches the shipped
`tessera tokenise` for segmentation (decision 0070), doing its own set-and-sequence arithmetic over
the token stream. Both are the right trade, argued rather than hidden. F4 below is about the
closure of the first one being narrower than its docstring claims, not about the trade.

**The largest finding is absence, as it was on every Rust surface.** The four definitional *value*
oracles — `viewport`, `mask`, `filters`, `text`, together ~1,150 lines and the substance of the I1,
I7 and I12 rows — have **no test of their own anywhere**. Each is checked only against the
implementation it exists to check. Reading closes most of the risk (the constructions genuinely
differ, and the negative controls bite) but it cannot close the mirrored-defect subclass by
construction, and that subclass is P2's whole premise.

**The differential families can fail, and §0's claim stands as written.** Every family I could
trace has something that makes it fail, and the narrowing §0 attaches to the claim is honest:
several structural checks are pass-only, and I name them below. The I7 negative control is the
strongest thing in either suite — it requires the first-*k* stub to serve the same *count* and to
be disagreed with on **more than half** the tiles, with a stated reason why "at least one" would be
a coincidence rather than a check (`conformance/tests/test_i7_selection.py:339`).

**`reference/tests` runs 71 tests; 62 need no corpus, and none of the 62 runs in CI.** Measured at
the base commit: `test_journal` 7, `test_wire_example` 7, `test_oracle_layering` 2, `test_identity`
21 of 24, all green in 21 s; `test_fixture_recipe` 25 green in 0.14 s. The nine that error are
`test_differential` (6) and `test_identity`'s three `fixture_bundle` cases, all of which build from
the Phase 0 corpus, which is not in the repository — expected, and not a failure.

**Six findings: one S3 that gates nothing, three S3s of substance, two S4s.** No vacuous check and
no unreachable test was found *inside* the oracle's own suites; the one reachability finding is
about the suites as a whole, not about a case within them.

## Findings

### F1 — `reference/tests` is excluded from CI wholesale, and only two of its six modules need the reason

**Claim:** the workflow's stated reason for not running `reference/tests` is true of two modules and
is applied to all six, leaving 62 corpus-free tests — including the oracle's only known-answer test
of the identity permutation and its only test of `AckedJournal`'s rules — gated by nothing.

**Evidence.** `.github/workflows/ci.yml:12`:

> `reference/tests` is NOT run here — two of its five modules build from that corpus, and
> repointing them would cost the viewport differential its realistic term distribution.

There are six test modules, not five, and the corpus dependency is confined to
`tests/test_differential.py` and three cases of `tests/test_identity.py`. Measured at the base
commit with the Phase 0 corpus absent: `pytest tests/test_journal.py tests/test_wire_example.py
tests/test_oracle_layering.py tests/test_identity.py` → **37 passed, 3 errors**;
`pytest tests/test_fixture_recipe.py` → **25 passed**. The 62 include every known-answer vector for
`oracle/identity.py`, whose independence from `crates/tessera-types/src/identity.rs` is the sole
reason their agreement over `reference/vectors/tessera_id.json` is evidence of anything — the Rust
half of that pair *does* run in CI (`crates/tessera-types/src/identity.rs:246`), so today only one
side of the cross-implementation check is gated.

**Class:** unreachable. **Severity:** S3.

**What a defect would let through.** A drift in `oracle.identity` reaches CI as a red *conformance*
test — `verify_identity_cross_check` inside `test_mask_catalogue.py` — and reads as an engine
defect. A regression in `AckedJournal`'s one rule (only a 200 is journalled; `_reflects` is
all-of and not any-of) composes a mask the service never promised and blames the engine for the
disagreement. Both are misattribution rather than leak, which is why this is S3 and not S1.

**Confidence:** high — measured, not read.

**Disposition:**

### F2 — the four definitional value oracles have no test of their own

**Claim:** `oracle/viewport.py`, `oracle/mask.py`, `oracle/filters.py` and `oracle/text.py` are the
second statement of record for §7.2's selection, I1's composition, decision 0062's filter tree and
records §10's text predicates, and nothing anywhere asserts a single one of their outputs against
anything but the engine.

**Evidence.** `reference/tests` contains no module naming any of the four; `oracle.filters` is
imported only by `conformance/conftest.py:83` and four `conformance/tests` differential modules,
`oracle.text` only by the two text modules, `viewport.Selection` only by differential drivers.
Neither `theta_cut` (`viewport.py:110`), `mask_of` (`mask.py`), `matches` (`filters.py:323`) nor
`TextColumn.has_phrase` (`text.py`) has a known-answer or property assertion. `filters.py` is 377
lines and carries several rules that exist nowhere else in Python — `none_of`'s presence
requirement (decision 0066, `filters.py:340`), the empty-combinator identities, the
`isinstance(operand, bool)` guard that stops JSON `true` resolving as vocabulary code 1
(`filters.py:118`).

**Class:** missing. **Severity:** S3.

**What a defect would let through.** A rule the oracle and the engine got wrong the same way. The
differential then agrees and reports conformance — the exact failure mode this track exists for.
Three things bound it rather than close it, and they are why this is S3: the two constructions are
genuinely different on every quantity that matters (recurrence vs closed form, per-entity walk vs
masked scan with derived postings, planted strings vs dictionary ordinals), so a *mirrored*
arithmetic error is unlikely rather than merely undetected; several of the sharpest rules have a
second statement on the Rust side, e.g.
`crates/tessera-engine/tests/filtering.rs:1683 none_of_requires_a_value_rather_than_taking_the_complement`;
and the viewport differential runs in two configurations with a non-vacuity guard
(`test_differential.py:245`'s `saw_partial`) plus the first-*k* negative control. What none of that
supplies is a statement of any of these definitions that is independent of *both* implementations.

**Confidence:** high on the absence; moderate on the consequence.

**Disposition:**

### F3 — the layering check that guards the oracle's independence is a hand-maintained allowlist

**Claim:** `test_oracle_layering.py` enumerates twelve module names literally, so a module not on
the list is checked by nothing — and two of the fourteen are already not on it.

**Evidence.** `reference/tests/test_oracle_layering.py:20`–`:22`:

> `DEFINITIONAL = ("viewport", "mask", "morton", "identity", "bundle", "wire", "filters", "record_blob")`
> `DRIVERS = ("harness", "journal")`
> `FIXTURE_BUILDERS = ("catalogue", "canary_fixture")`

`:56` iterates `DEFINITIONAL` alone. `oracle/text.py` and `oracle/label_fixture.py` appear in none
of the three, and `oracle/__init__.py`'s three-way taxonomy — the prose this test exists to make
falsifiable — omits the same two. So the property `conformance.md` §1 calls out as the oracle's own
best addition ("the three-group layering is enforced by a test, not by prose") does not hold over
the whole package, and a new definitional module that imported `harness` tomorrow would pass. The
second test in the file (`:65`, every third-party import declared) globs `ORACLE.glob("*.py")` and
does not have this shape, which is the construction the first one wants.

This is the allowlist failure `oracle/catalogue.py`'s own module doc argues against for fixture
reuse — "a hand-maintained 'does it look right' test is an allowlist that has to be extended in
step with every new input, and the input that is *not* on it is exactly the one that goes silently
wrong" — applied to the reuse receipt and not to this.

**Class:** under-discriminating. **Severity:** S3.

**What a defect would let through.** A definitional module acquiring a dependency on the system it
measures — the one thing this check exists to catch — provided the module is not on the list.
Neither `text.py` nor `label_fixture.py` violates the rule today (`text.py` takes the binary path
as an argument and finds nothing; `label_fixture.py` is a fixture builder and imports `harness`
legitimately), so this is a check that cannot fail rather than a violation it missed.

**Confidence:** high — the list is a literal tuple.

**Disposition:**

### F4 — `viewport.py`'s closure of its one shared artefact does not cover the bundle its own differential runs on

**Claim:** the module doc says the stored `tessera_id` column is checked so that "nothing about it
is being taken on trust" by the time a `Selection` reads it; the viewport differential in
`reference/tests` builds `Selection`s over a bundle where none of those checks is run.

**Evidence.** `reference/oracle/viewport.py:75`–`:79`:

> `Bundle.verify_identity_cross_check` proves the stored column *is* `forward(key, shard,
> entity_of_row)` for a sample of rows, `Bundle.derive_row_order` proves the rows are stored in the
> order that key implies, and the fixture — not the bundle — supplies the key.
> `conformance/tests/test_mask_catalogue.py` runs all three against the catalogue bundle, so by the
> time a `Selection` reads the column, nothing about it is being taken on trust.

True of the catalogue bundle: `test_mask_catalogue.py:53`, `:55` and `:60` run all three, the third
comparing MANIFEST's key against `cat.CATALOGUE_ID_KEY_HEX`, which the fixture puts in the build's
environment. `reference/tests/test_differential.py` runs the §7.2 viewport differential over the
250k `--mint-id-key` fixture and calls **none** of the three; that fixture's key is a build output,
which `test_mask_catalogue.py:49` states in as many words ("ran only against the 250k
`--mint-id-key` fixture, whose key is a build output rather than a fixture input"). So the sentence
is scoped to one bundle and reads as scoped to the machinery.

**Class:** mis-named. **Severity:** S3.

**What a defect would let through.** On the 250k bundle only: a build writing a wrong-but-self-
consistent `tessera_id` column — the r21 disclosure the negative control's own docstring invokes,
an identity still correlated with term-signature order — is agreed with rather than caught, because
both sides read the same wrong values. §4.6's I7 evidence is the catalogue differential, where the
hole is closed, so no coverage row is affected.

**Confidence:** high.

**Disposition:**

### F5 — `mask.ChangeSet`'s `∪ direct_eval(L)` arm is unreachable, and the module doc presents it as the composition under test

**Claim:** `predicate` is withdrawn (decision 0047) and the server refuses it with a 422, so no
acked predicate op can exist, so `ChangeSet.overrides` is always empty and `resolve` reduces to
`base_mask - deleted - suppressed`. `mask.py` still states I1's composition as
`M_auth = (mask \ L) ∪ direct_eval(L)` with `L` as its subject.

**Evidence.** `reference/oracle/mask.py:10` and `:67` carry the formula; `:62` and `:68`–`:71`
implement the override arm. `crates/tessera-server/src/control.rs:1881` refuses `"predicate"`, and
`crates/tessera-lifecycle/src/wal.rs:200` records the variant deleted at `WAL_VERSION` 5.
`conformance/tests/test_overlay_journal.py:719` drives the refusal deliberately and `:259` records
that the state "used to carry a predicate-widen as well, for the `∪ direct_eval(L)` arm" and no
longer does. The journal's submission path is still needed for the refusal test; the *composition*
arm is not, and is exercised by nothing.

**Class:** vacuous. **Severity:** S4.

**What a defect would let through.** Nothing today, since the arm cannot be entered. It is here
because a reader of `mask.py` takes the module doc for a description of what the I1 differential
composes, and `L` has been empty in every run since 0047. `CLAUDE.md`'s pre-release rule — do not
carry a shape whose only justification is a state some earlier version could have produced — points
at deletion rather than at a test.

**Confidence:** high.

**Disposition:**

### F6 — the quantiser on the oracle's geometry path is a second implementation nothing pins

**Claim:** `bundle.py` says the definitions stay scalar in `morton.py` and only the extraction is
vectorised; the quantisation itself is re-implemented in numpy, `morton.fixed32` is not called on
that path at all, and the pin the copy's docstring names is against the engine rather than against
the definition.

**Evidence.** `reference/oracle/bundle.py:168`:

> The definitions themselves (`fixed32`, the interleave) stay scalar in `morton.py`; what is
> vectorised here is only the extraction.

But `:219`–`:220` quantise with `_fixed32_vec`, and `_fixed32_vec` (`:228`) is a separate
implementation of the same clamp-and-floor. Its own docstring at `:229` says it is "Pinned against
the scalar definition by `test_differential`'s byte-for-byte position check" — that check
(`test_differential.py:105`) compares this function's output against **what the engine stored**, not
against `morton.fixed32`. Nothing anywhere calls `morton.fixed32` on the oracle's differential path;
its only reachable call site is `morton.code_of`, used by `conformance/suite/verification.py:377`,
which `conformance.md` §0 records as not run. The interleave genuinely is shared — `split32` at
`:442` is the scalar one.

**Class:** mis-named. **Severity:** S4.

**What a defect would let through.** Little, and in the safe direction: the two differ today only
on NaN (the scalar raises, the vector produces an undefined `uint32`), and any real divergence
surfaces as an oracle-vs-engine position disagreement at `test_differential.py:105` rather than as
a silent pass. The finding is that a reader is told the definition is the thing running, and it is
not.

**Confidence:** high.

**Disposition:**

## Independence, module by module

| Module | Derived from first principles | Read back from the system | Verdict |
|---|---|---|---|
| `morton` | quantisation, interleave, tile enumeration, from contracts §2.5's byte-level definition | nothing | **independent** |
| `identity` | the Feistel network from the construction memo §1, explicitly not from `identity.rs` | nothing | **independent**, and the only module with a third-party pin (the JSON vectors both implementations read) |
| `mask` | the viewer's set by direct scan of `terms/pairs.parquet` | nothing — the engine's route is `postings.arrow` | **independent** |
| `viewport` | §7.2 as a literal sort/count/slice; θ as the recurrence; the anchor from the segment and the mask | the stored `tessera_id` column — the sort key, declared at `viewport.py:63` | **independent with one declared echo**; closed on the catalogue bundle, open on the 250k one (F4) |
| `filters` | decision 0062's tree as a per-entity walk over the fixture's generation functions | nothing — `attrs/` is never opened | **independent** |
| `text` | m-of-n and phrase arithmetic over the fixture's prose | tokenisation, from `tessera tokenise` (decision 0070) | **independent on the part under test**, echo declared and argued |
| `bundle` | manifest and file parsing from contracts §2.1–§2.4; every digest verified; row order re-derived from source geometry plus the permutation | the stored `tessera_id` column (handed on to `viewport`); postings, as the artefact `catalogue.verify()` checks *against* the fixture | **independent**, with the geometry fallback refused rather than defaulted |
| `record_blob` | the blob's framing from records §3, structure only — no function returns a field's value | the artefact's own directory, checked for self-consistency and against the fixture's has-row set | **independent in the strong direction** (artefact vs fixture) |
| `wire` | the frame grammar and the trailer's closed key set from contracts §3.2 | the response bytes, which is its job | **independent**; mirrors `decode_viewport_frames`'s strictness, not its code |
| `catalogue` | a synthesised corpus and its attribute values as pure functions of source id | the built bundle, in `verify()` — which is the fixture-vs-artefact direction | **fixture builder**; `verify()` checks the interning **rule** and, since check 3b, source-to-entity placement item by item |
| `canary_fixture` | three corpora and the five allocation rules | both bundles' stored rows, compared against each other in `verify_allocation_rules` | **fixture builder**; the comparison is build-vs-build on purpose and says so |
| `label_fixture` | one corpus, three generating sets, `visible_to` from the planting rules alone | nothing | **fixture builder**, and never joins planted source ids to entity space, so 0073's trap does not reach it |
| `journal` | the acked-only rule, in entity space | the control plane's status codes and `entity_id_high_water` — its job, and the proxy is named as one at `journal.py`'s `barrier` | **driver** |
| `harness` | nothing definitional | the whole system | **driver**; reuse decided by a stamped input set rather than by a predicate over the artefact |

## Which differentials can fail, and which checks cannot

**Can fail, with a live control:**

- **I7 / selection** — `first_k_rows` serves §7.2's *count* in storage order; the test asserts the
  stub's count matches (or the comparison would be arithmetic, not membership) and requires
  disagreement on more than half the tiles. The strongest control in either suite.
- **I1 / overlay journal** — two deliberately defective engines, one sampling from a pre-overlay
  mask, one anchoring θ on a pre-overlay projection, both asserted rejected.
- **I2 / canary** — the visible-items state is a positive control the same comparator must reject,
  and `verify_allocation_rules` is a real re-derivation (row-for-row equality plus exactly one row
  at the maximal corner) rather than a description.
- **Viewport / θ** — `require_partial` at `test_differential.py:245` fails the test if no tile ever
  served strictly fewer points than it had visible, which is what stops the live-θ case reverting
  to the saturated one.
- **I3 / label containment** — `l-core` served to the narrower principal in the same response is a
  positive control for `l-whole`'s absence, and the one-entity difference is asserted from the
  engine's own masked counts before anything rests on it.
- **I10 / byte scan** — a planted emission per sweep mechanism *and* a clean payload each must
  return nothing.
- **Mask catalogue** — `verify()` reports every drift, and check 3b compares the two id spaces item
  by item, which is precisely the check a set comparison could not make.

**Pass-only, and §0 is right to narrow its claim:**

- `test_mask_catalogue.py:33` `verify_identity_cross_check` and `sidecar_round_trips` — both raise
  on disagreement, so they can fail, but neither has a damaged input that demonstrates it; they are
  self-consistency checks over one build's outputs.
- `test_differential.py:105`'s position check strides `row_count // 5000` rows; a defect confined to
  unsampled rows passes. No guard states the sampling rate as a limit.
- `record_blob.self_check` is only ever asserted empty; nothing feeds it a deliberately mis-addressed
  blob, so the walk's own discrimination is untested. (Its structure makes a planted defect easy —
  this is a gap, not an impossibility.)
- The `_verify_files` digest sweep in `Bundle.__init__` fires on every open and has no negative
  case anywhere.
- `test_oracle_layering.py`'s first test — F3.

## What was checked and found sound

- **The geometry input is genuinely upstream of the build.** `Bundle._require_source` refuses
  rather than falling back to `morton.u32`/`residual`, and the refusal message says why. This is
  the one construction that would have made every position comparison a tautology, and it is
  closed structurally rather than by convention. The unbound points file (`⊘` at
  `bundle.py:100`) is a real gap, is marked, and is the harness's discipline by design.
- **The fixture-reuse receipt.** `harness.fixture_recipe` stamps the whole input set — argv,
  declaration text, and a size/mtime stamp per input file — and `_fixture_bundle_is_usable` adds a
  structural gate and a "no server has published into this" gate. `test_fixture_recipe.py`'s 25
  cases include one per input the fixture is a function of, and one that a bundle a server has
  written into is not reused. This is the strongest single piece of engineering in the track and it
  fixed a defect that had happened twice.
- **`catalogue.verify()` after r15.** The recorded `verify()` defect is genuinely fixed, not
  papered over: the interning check now tests the *rule* (`public` at term 0, offset derived from
  it) rather than an assumed equality, and check 3b compares source and entity space per item.
  `high_tail`'s absence is checked rather than skipped, with a comment saying that
  `blocks.get(..., set())` followed by `if high_tail` would have made a deletion a silent pass —
  the same class of thought this campaign is made of.
- **`test_the_catalogue_covers_the_properties_the_design_names`** writes the expected list out
  rather than importing it, on the stated ground that importing it would make both sides come from
  one edit. That is the correct answer to the tautology question and it is argued at the test.
- **`journal.AckedJournal`** is tested against a stub control plane for exactly the states a real
  deployment will not produce on demand — a 409 batch journalling nothing at all, a barrier that
  times out, `_reflects` being all-of and not any-of. The 200-only rule is asserted in both
  directions.
- **`wire.decode_frames`** refuses rather than tolerating: a truncated body, an unknown kind, a
  second tiles or artifacts frame, a trailer key outside the closed set, a shape with one axis
  column and not the other, and a trailer whose `points` disagrees with the body. Four of these have
  tests in `test_wire_example.py`.
- **`identity.py`** is the best-tested module in the package: 24 cases including both directions
  over the vectors, a secondary key asserted to *disagree* with the canonical one, the degenerate-key
  gate proven unbypassable by direct construction, and two tests that key material reaches neither a
  rejection message nor a `repr`.
- **The `region` leaf's polygon walk** is duplicated between `filters._inside_polygon` and
  `conformance/tests/test_shape_membership.py:182`, which is a drift risk rather than a defect: both
  are compared against the engine, and the tie-rule argument (half-open ray inside, engine symbolic
  perturbation, agreeing on and off the boundary by construction) is stated and correct. Worth one
  edit to share, not a finding.
- **No `assert True`, no self-comparing assertion, no commented-out test, no bare `except: pass`**
  anywhere in `reference/`. Every zero-assertion body delegates to a helper that raises.

## Two things this track did not settle

**`conformance.md` §0 and §1 describe a suite smaller than the one that exists.** §0's table says
"one flat `conformance/tests/`, seven pytest modules" and §1's `⊘` names the seven; there are
**seventeen**, the extra ten being the keyword, text, layering, shape-membership, region-leaf,
label-containment, record-blob and schema-refusal modules. This is the direction `CLAUDE.md`
singles out — a register that records built machinery as absent understates its own gap — and it is
adjacent enough to §4.6 that I have not touched it. Note that §4.6's I3 row already cites
`test_label_containment.py`, so the matrix is ahead of §0's inventory.

**`reference/oracle/harness.py:722` writes `min_visible_members = 10` into every server config, and
the key no longer exists.** Decision 0085 removed `Config::min_visible_members`;
`[disclosure]` is parsed as a raw `toml::Value` table (`crates/tessera-server/src/config.rs:2007`)
rather than under `deny_unknown_fields` like every other section, so the line is accepted and
inert and nothing says so. No test depends on it, so nothing passes vacuously today — but a test
written tomorrow against a deployment-wide existence threshold would. The campaign tracker records
this hazard as "superseded, not outstanding"; superseded in the engine, still written by the
harness. Not escalated as a defect: it discloses nothing and refuses nothing.
