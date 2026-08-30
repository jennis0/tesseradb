# Test audit R8 — `tessera-server` and `tessera-cli`

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests, not
the code under them**: nothing below is a claim that shipped behaviour is wrong, and no defect in
shipped code was found. Track R8 of Wave 1 of the test-quality campaign
([`test-audit-campaign.md`](../../test-audit-campaign.md)); Wave 0's
[reachability memo](2026-08-30-test-reachability.md) is the input for which tests run, and the
[access-control red-team](2026-08-14-access-control-redteam.md) is the input for which attacks on
this surface have already been run. `conformance.md` §4.6 is untouched — a row moves only when a
test moves with it, and no test moved.

## Results

**366 tests assessed and the surface is healthy** — `cargo test -p tessera-server -- --list` reports
**314** and `-p tessera-cli` **52**, across fourteen server integration binaries, seven `src`
test modules (`cors`, `health`, `error`, `state`, `config`, `filter_dto`, `control`), five CLI
integration binaries and no CLI `src` tests at all. This is the trust boundary itself and it is
tested like one: the strongest convention here is that a test **states what it cannot guarantee**
and names the structural mechanism that covers the rest, which is how
`every_control_route_requires_the_operator_credential` earns its hard-coded route list.

**The harness is sound, and it is the opposite of the failure mode the brief anticipated.**
`crates/tessera-server/tests/common/mod.rs` normalises nothing. It sorts no list, defaults no
missing field, retries no failure and unwraps to no default. What it does instead is **add**
assertions that every test above it inherits: `decode_viewport_frames` enforces the frame grammar
of `contracts §3.2 r26` on every response any test decodes — tiles first (`:690`), exactly one
trailer last (`:695`), the sub-cells frame immediately after tiles (`:730`), at most one artifacts
frame (`:746`), an unknown kind a panic (`:961`) — then closes the trailer's key set (`:957`) and
cross-checks three things the body claims against what it carried: the trailer's point total
(`:971`), its flush count (`:976`) and `served` summing to the points delivered (`:982`, r7's own
invariant, asserted at the reader). `authorise` asserts its own 200 (`:574`). The artifacts decoder
reads `rung` and `matched` **positionally** and says why in a comment: reading by name would not
notice a column inserted ahead of them, and the fixed prefix's positions are contract. Where the
harness narrows a fixture it says so at the line and gives the reason —
`max_category_values: 4` so vocabularies page under ordinary requests, `theta_target_marks:
u64::MAX` so these tests assert HTTP shape and masking rather than density, generous gate and
ingest bounds so that only the bound-specific tests can observe a bound. **The one thing a reader
should carry away is that θ is saturated in every server test**: the density clause of design §7.2
is not exercised anywhere in this crate, deliberately and by the engine's arrangement, so the
per-tile mark counts these tests see are `min(k, visible)` and nothing else. The cap clause *is*
exercised (`k = 5` over 1,000 items), so sampling-after-masking is not degenerate here.

**Four findings, none of them in the harness.** Three are *missing* and one is *mis-named*; none is
unreachable and none is vacuous. The largest is L4's: the viewer plane's credential requirement —
the one claim on this surface whose failure is an unauthenticated read of a whole record — is
tested on three of its five routes and on none of them by enumeration, where the control plane
next door has exactly that test and the comment explaining why it was written.

**Every finding was established by reading.** Mutation was not used: in each case the absence is a
grep over the crate's own request sites rather than a question about behaviour, and the campaign's
owner ruling of 2026-08-30 is that certainty from reading needs no mutation.

## Findings

### F1 — no test asserts that `/v1/items` or `/v1/artifacts` requires a session credential, and nothing enumerates the viewer plane

**Claim:** `contracts §3.2` authenticates every viewer data route by session bearer; two of the four
have no test of it, and unlike the control plane there is neither a router-wide layer nor a
route-enumerating test to cover a route that forgets.

**Evidence.** The viewer router mounts five routes and a pair of probes with **no credential layer**
(`crates/tessera-server/src/viewer.rs:45`–`:53`); each handler carries its own check —
`bearer_token(&headers).ok_or(ApiError::BadCredential)?` at `:145` (`meta`), `:441` (`categories`),
`:1273` (`viewport`), `:1790` (`artifact`) and `:1832` (`item`). Three of those checks have a test:
`crates/tessera-server/tests/http.rs:532` (`viewer_meta_requires_bearer`),
`crates/tessera-server/tests/http.rs:399` (`b_missing_or_garbage_token_is_401`) and
`crates/tessera-server/tests/categories.rs:591` (`the_route_requires_a_session_token`). **Two do
not.** `crates/tessera-server/tests/openapi.rs:826`
(`items_match_the_description_with_every_refusal`) exercises 404, 409 and a schema refusal and no
401; `:872` (`artifacts_match_the_description_with_one_refusal_shape`) exercises three 404s and a
409 and no 401 — its own name says *one refusal shape*. Every one of the eighteen `/v1/items` or
`/v1/artifacts` request sites in the crate, `crates/tessera-server/tests/common/mod.rs:582`'s
shared `post_item` included, supplies a bearer.

The contrast is the point, and it is written down in this repository already.
`crates/tessera-server/tests/http_write.rs:3425` exists because

> `control_status_requires_bearer` names one endpoint and would stay green forever while a fourth
> control route shipped wide open; that is exactly how `/control/status` itself shipped returning
> `entity_id_high_water` unauthenticated.

That test iterates `CONTROL_PLANE_ROUTES` with no exemption list, and its doc is explicit that the
hard-coded list cannot see a route nobody added to it — for which the cover is
`require_operator_credential` wrapping the whole router. **The viewer plane has neither half.** A
route added to `viewer::router` without its own `bearer_token` line is caught by no layer and by no
enumeration.

**Class:** missing. **Severity: S1** — a disclosure claim with no test, on the two routes that
return an item's whole record and an artifact's derived geometry.

**What a defect would let through:** a viewer route serving content to a caller with no session.
Two things hold the practical risk below the severity and both belong in the record. Every handler
follows its `bearer_token` line immediately with `state.authenticated_session(token)?`
(`crates/tessera-server/src/viewer.rs:1833`), which refuses an unknown token, so *deleting the
first line alone* still refuses — the exposure is a **new** route written without either. And the
control-plane test above is the model for the fix: a `VIEWER_PLANE_ROUTES` constant and the same
loop would be a handful of lines, and would also pin the plane's second half — that `/healthz` and
`/readyz` are the only two routes on it that answer without a credential
([decision 0011](../../decisions/0011-health-probes-off-control-plane.md)).

**Confidence:** high, by reading. Mutation not used — the absence is a grep over the crate's own
request sites, and the handlers were read to confirm this is a test gap and not a live defect.

**Disposition:**

### F2 — the sub-cells frame's "presence follows the request, not the result" rule is tested only where the result is non-empty

**Claim:** `contracts §3.2` item 2 requires that *a request that asks for the underlay and yields no
cells gets a present frame carrying a schema-only, zero-row stream — never an absent frame*. No
test anywhere in this crate takes an underlay request that yields no cells.

**Evidence.** The rule is implemented, and the code says so at the line
(`crates/tessera-server/src/viewer.rs:848`):

> `// `Some` of an empty slice is a present, zero-row frame; `None` is no frame at all —`
> `// presence is decided by the request, not the result (contracts §3.2's r12 rule).`

with the emission at `:849` and the engine's half — `underlay_offset.map(|_| sub_cells.as_slice())`
— at `crates/tessera-engine/src/viewport.rs:2382`. The three tests that touch the frame's presence
all sit on the easy side of the rule: `crates/tessera-server/tests/openapi.rs:709` asserts
`sub_cells.is_some()` on a request whose underlay returns cells, `:756` asserts `is_none()` on a
request that asked for none, and `crates/tessera-server/tests/http.rs:1936` compares
`chunked.sub_cells` against `whole.sub_cells` where both are `None`, so that line of that test
proves nothing (its other three comparisons do). The harness distinguishes the two states —
`DecodedViewport::sub_cells` is `Option<Vec<_>>` — so the assertion is available and simply not
written.

The uncovered case is reachable through the ordinary surface: a principal whose mask is empty over
the requested tiles, or a bbox over an unoccupied corner, with `underlay_offset >= 1`.

**Class:** missing. **Severity: S3** — a real defect could pass. Not S1: an absent frame would leak
nothing the *tiles* frame does not already disclose exactly (`visible = 0` for the same principal
over the same tiles, §7.1), so what breaks is the wire contract and a positional decoder, not the
mask.

**What a defect would let through:** a change that emitted the frame only when it had rows —
`if let Some(cells) = sub_cells` narrowed to `if let Some(cells) = sub_cells.filter(|c|
!c.is_empty())`, which reads as a tidy-up. Every test in the crate stays green, and a client that
reads frames positionally against its own request finds the kind-2 slot missing exactly when the
answer is *no marks here*.

**Confidence:** high, by reading. Mutation not used — the enumeration of the three sites that touch
`sub_cells` is complete and each was read.

**Disposition:**

### F3 — two engine refusals that must be `422 contract` have no test of their HTTP mapping, behind a catch-all that makes 500 the failure mode

**Claim:** `contracts §3.1`'s code list makes a bounds refusal `422 contract`, which is the *shape*
class a client must not retry unchanged; `crates/tessera-server/src/error.rs` maps two such engine
refusals arm by arm and neither arm is covered.

**Evidence.** `map_engine_error` carries `EngineError::UnderlayRefused(detail) =>
ApiError::Contract(detail)` at `crates/tessera-server/src/error.rs:332` and `too_many @
EngineError::TooManyTiles { .. } => ApiError::Contract(...)` at `:340`, with a catch-all
`other => ApiError::FailClosed(other.to_string())` at `:377`. The module's own unit tests exercise
five arms — `Io`, `ProjectionBuilding`, `FragmentBuilding`, `StaleIdSet`, `Cancelled` — and neither
of these two. End to end the picture is the same: `crates/tessera-server/tests/openapi.rs:784`–`:793`
covers `zoom > 16` (refused in the server at `crates/tessera-server/src/viewer.rs:1276`, not by the
engine) and an unknown filter column (`FilterMalformed`), and no test in either crate sends an
`underlay_offset` over `max_underlay_offset`, an offset taking `zoom + offset` past 16, an
over-budget cell demand, or a tile count over `max_tiles_per_request`.

**The refusals themselves are covered, one crate over.** `crates/tessera-engine/tests/viewport.rs`
matches `EngineError::UnderlayRefused` on all three underlay bounds at `:1578`, `:1592` and `:1608`,
and destructures `TooManyTiles { demanded, limit }` at `:1647`. What is untested is only the
translation into the status code the contract names.

**Class:** missing. **Severity: S3** — a real defect could pass. Never fail-open: with the arm gone,
the catch-all answers `500 fail-closed`, which discloses nothing and refuses the request.

**What a defect would let through:** a refusal in the wrong class. `contracts §3.1` groups the
codes so a client knows what to do — *shape* must not be retried unchanged, *failure* is
fail-closed and final — so a bounds refusal answering 500 tells a client its own arithmetic was
fine and the server broke, and the shipped client has no way to learn otherwise.

**Confidence:** high, by reading; the arms and both crates' tests were enumerated.

**Disposition:**

### F4 — a test's doc says it is ignored and would fail; it is neither

**Claim:** `an_omitted_layers_field_means_no_artifacts_frame` documents itself as an `#[ignore]`d
test asserting behaviour this tree's server does not have. It carries no `#[ignore]`, it runs, and
the behaviour landed.

**Evidence.** `crates/tessera-server/tests/openapi.rs:800`:

> `/// The server change lands on the s3 track; until it merges an omitted field is`
> `/// read as `"all"` and the string is a `422`, so this test would fail against the server here.`
> `/// Ignored with that reason, for the controller to enable at integration`

The attribute below it is `#[tokio::test]` and nothing else — Wave 0's sweep found **no** `#[ignore]`
anywhere in `tessera-server`, and `crates/tessera-server/tests/viewport_membership.rs:249`
(`omitted_layers_means_none_and_the_word_all_means_every_reachable_layer`) pins the ruled behaviour
independently, so the server change plainly merged. The `#[ignore]` was removed at integration and
the doc was not.

**Class:** mis-named. **Severity: S4** — weak but misleading in the one direction that matters for a
doc: a reader triaging this file is told a green test is disabled and that the server behaves the
opposite way to how it does.

**What a defect would let through:** nothing directly. What it costs is a reader's time, and the
`⊘`-style habit the corpus depends on — a marker that has stopped being true is worse than no
marker, because the next reader reasons from it.

**Confidence:** high, by reading.

**Disposition:**

## The four handed items

**1. The byte-identity pair — the names are now honest.** `bench-timing` is on the server's self
dev-dependency (`crates/tessera-server/Cargo.toml`, `[dev-dependencies]`), and `cargo tree -p
tessera-server -e dev -f "{p} {f}"` resolves the package as `bench-timing,default,fault-injection`
under `-p` as well as `--workspace`. Integration tests compile with the package's active feature
set, so the `#[cfg(feature = "bench-timing")]` blocks at
`crates/tessera-server/tests/http.rs:1571` and `:1708` are live in both selections, and
`Engine::set_serial_fallback_max_rows_for_test` (`crates/tessera-engine/src/session.rs:1522`,
itself `bench-timing`-gated) forces both servers' threshold to 0 before either is mounted. The
threshold is read from the engine and not from the constant
(`crates/tessera-engine/src/viewport.rs:2326`), so both servers genuinely take `pool.install` and
the comparison is parallel-at-1-worker against parallel-at-8. **Wave 0's finding is closed and both
tests now check what their names claim.** One residue: each doc still carries the paragraph
describing the without-`bench-timing` fallback to "comparing the serial fold on both configs".
That sentence is true as a conditional and now describes no selection in the repository; it is not
a finding, but it is the sentence a reader will quote if the dev-dependency is ever trimmed.
**No siblings.** The only other two-servers-compared test in the crate is
`a_tiny_flush_threshold_streams_many_point_frames_with_identical_content`
(`crates/tessera-server/tests/http.rs:1878`), whose two servers differ in a flush threshold that is
asserted to have taken effect (`point_frames > 1` against `== 1`) before anything is compared — the
non-vacuity guard the shape needs.

**2. The harness — sound, and the highest-confidence verdict in this memo.** Read in full; the
account is in Results above. It is 1,052 lines of which the bulk is one decoder that *raises* the
assertion floor for every test above it. Three specific things a future reader should not undo: the
trailer key-set closure (`:957`), which is the only thing standing between the one server-authored
JSON region of the body and a field the conformance comparator never sees; the positional reads of
`rung` and `matched` (`crates/tessera-server/tests/common/mod.rs:919`, `:924`), which are what make an inserted column a failure rather than
a silent rebinding; and the doc on `TestServer::state` (`:161`), which grants engine-side
observation and then forbids using it to drive a request — *"a test that drives the engine through
this field instead of through a request has stopped being a server test"*. The only harness-wide
narrowing is θ saturation, declared at the line.

**3. Appendix C at the boundary — covered where it can be, and the gaps are elsewhere.** Walked row
by row against the tests. **C11** (a vocabulary offering values the viewer cannot see) is the
best-covered row on the surface: `crates/tessera-server/tests/categories.rs` filters per principal
(`:469`), applies the gate before the two request forms diverge (`:492`), fills a page *after* the
gate so a short page is never itself a count (`:523`), and serves an empty set with 200 to a
principal who may see none (`:552`) while the `public` column stays whole for that same principal
(`:564`) — which is **C24**'s authored-assertion half. **C8** is covered by absence-of-surface: the
category route returns `{code, key, label?}` and no count, and `filter_operands` publishes no
cardinality. **C17** is pinned at `crates/tessera-server/tests/http.rs:323`, which compares the two
404 bodies **byte for byte** and says why, and at `layers.rs:433`, which asserts the artifact
identifier is stable across a broad and a narrow principal while only the count moves. **C29**
(a parent identifier on the wire) is covered one crate over —
`crates/tessera-engine/tests/artifact_projection.rs:373` asserts every re-rooted leaf carries
`parent_id: None` — and is reachable at the wire only through
`crates/tessera-server/tests/membership_column.rs:642`, which reads column 11 directly; the shared
harness decoder does not read `parent_id` at all, so no test using it can observe that column.
That is worth knowing and is not a finding, since the column's position is pinned by the
membership-column decoder and its semantics by the engine. The remaining rows are either timing
channels the register accepts and no test can assert (**C4**, **C14**, **C19**, **C21**, **C25**,
**C26**), rows about machinery that does not exist (**C13**, **C16**, and I13b's compartments),
or rows the register's own preamble marks as notes rather than controls (**C2**). The red-team's
third latent hazard — `min_visible_members` parsed and never enforced — is **superseded**, not
outstanding: the key is deleted under [decision 0085](../../decisions/0085-the-existence-criterion-has-no-deployment-wide-form.md)
and the control is the per-layer existence criterion. Its first and second hazards (the
`RowProjectionKey` invariant, and I13b/c) are unchanged and are not this track's to move.

**4. `tessera-cli` — the config surface is tested by kind, and `tessera serve` itself is not tested
at all.** The absence of `src` tests is not the gap it looks like: the CLI is argument parsing and
delegation, and the refusals that matter are `tessera_server::config`'s, which has **51** unit
tests and matches on the **variant** rather than on the fact of failure —
`ConfigError::MissingDisclosureSection`, `MissingDisclosureKey("token_max_lifetime")`,
`UnderlayOffsetTooDeep(17)` and the rest, each with the deployment file that produces it inline.
The whole crate holds exactly **one** bare `assert!(x.is_err())`, at
`crates/tessera-server/tests/http.rs:522` (`h_config_missing_disclosure_refuses_to_start`), and it
is the end-to-end sibling of `config.rs`'s variant-matching `missing_disclosure_section_refuses_to_start`
— weak on its own, not misleading, and covered next door; not reported as a finding. The identity
and key-source refusals, which are the *irreversible* half of `CLAUDE.md`'s three questions, are
covered by kind and by message in `crates/tessera-cli/tests/identity_cli.rs` — including
`the_rotation_refusal_prints_fingerprints_not_keys` (`:398`), which is a disclosure assertion in a
build tool. What has no test is the `Serve` arm itself
(`crates/tessera-cli/src/main.rs:1804`–`:1857`): four `match` arms that print
`tessera serve: refused to start:` and return `ExitCode::FAILURE`. It relays a decision made and
tested elsewhere and could only fail by ignoring an `Err` it visibly does not, so this is recorded
rather than reported.

## L1 — reachability, confirmed and quick

Neither crate holds an `#[ignore]`. The only feature-gated test code is the two `bench-timing`
blocks in `crates/tessera-server/tests/http.rs` (item 1 above) and the `fault-injection` gates in
`crates/tessera-server/src/lib.rs`, `src/control.rs` and `src/state.rs`, both features being named on the server's self
dev-dependency — so `-p` and `--workspace` select and compile the same set, and the resolver-2
coupling `test-audit-campaign.md` §8 records as systemic cannot bite here. Measured: `-p
tessera-server` lists 314, `-p tessera-cli` 52, `--workspace` 2,305 entries. Nothing further.

## L4 — the claims of `contracts.md` §3, checked one at a time

The absence lens was worked as an enumeration of the design's claims rather than as a reading of
the tests, because absence is the class a test-first pass cannot see. Each row is a normative claim
this crate owns; the third column is the test that would go red if it were violated.

| Claim (`contracts §3`) | Covered? | Where |
|---|---|---|
| §3.1 every 429 carries `Retry-After` and a body `retry_after_s` holding the same number | yes, at both timescales | `crates/tessera-server/tests/openapi.rs:79`'s `assert_refusal` checks the pair on every refusal it sees; the compute gate's fixed 1 at `http.rs:988`; ingest's *derived* figure at `http_write.rs:2779`, with the mutations it kills named at the test |
| §3.1 the value is fixed at 1 **only** for the compute-admission gate | yes | `openapi.rs:561` (exactly 1) against `http_write.rs:2559` (`>= 1`, derived from the queue's drain) |
| §3.1 `/control/changes` is never load-shed | yes | `http_write.rs:2615` and `crates/tessera-server/src/error.rs:1030` (`a_change_batch_never_answers_429`) |
| §3.1 the control plane is uniformly authenticated, no exemption | yes, both halves | `http_write.rs:3425` (every listed route, 401 body asserted field by field) and `:3504` (every path, routed or not) |
| §3.1 the probes are on viewer and session and **not** on control | yes | `openapi.rs:565`, `http_write.rs:1452`, and the control half falls out of `:3504` |
| §3.1 a 500 on `/control/changes` distinguishes *owed durability* from *refused* | yes, at depth | eleven cases in `crates/tessera-server/src/error.rs:822`–`:1023` covering every combination of applied/refused/lost-receipt |
| §3.1 error detail is never forwarded from a store, WAL, join or engine error | yes | `crates/tessera-server/src/error.rs:1056`, `:1072`, `:1090` and `:1111` |
| §3.2 `/v1/meta` reports the idset and never the identity key | yes | `http.rs:571` |
| §3.2 each `views` entry publishes projection, world aspect, tile scheme and tile | yes, all four projections | `meta_projection.rs:130`, `:182`, `:215`, `:243` |
| §3.2 `/v1/categories` filters `derived` per principal, before the two request forms diverge | yes | `categories.rs:469`, `:492`, `:523`, `:552` |
| §3.2 `/v1/categories` `public` is served as authored to any session | yes | `categories.rs:564`, in the same test as the derived principal's empty set |
| §3.2 an unresolvable code, the absent sentinel and an invisible value are one outcome | yes | `categories.rs:405`, all three codes in one probe |
| §3.2 key order, cursor resumes; `limit` clamps and `limit = 0` is 422 | yes | `categories.rs:426`, `:607` |
| §3.2 a plain scalar and an unknown column are the same 404 | yes | `categories.rs:574` |
| §3.2 `matched = visible` with no filters; `served` splits the points | yes | `http.rs:80`, and the harness asserts the split on every decode (`crates/tessera-server/tests/common/mod.rs:982`) |
| §3.2 the tiles form is answered in **request order** with first-occurrence dedup | yes | `http.rs:1943`, reversed request against reversed response |
| §3.2 exactly one of `bbox` and `tiles`; a prefix with bits above the depth is refused | yes | `http.rs:178` |
| §3.2 frame boundaries are not contract; content is | yes | `http.rs:1878`, a 1 KiB threshold against the 1 MiB default |
| §3.2 `k = 0` is the counts-only request — tiles, artifacts, no points frame | yes | `openapi.rs:740` |
| §3.2 the *artifacts* frame is **absent**, never empty, when nothing is served | yes | `layers.rs:481`, four ways (no layer, `[]`, an unreachable name, and the positive control) |
| §3.2 the *sub-cells* frame is **present**, never absent, when the underlay was requested | **half** | presence-with-cells at `openapi.rs:709` and absence-without-request at `:756`; the zero-cell case is **F2** |
| §3.2 `masked_count` is the asking principal's and never the artifact's | yes, from a narrow credential with a non-vacuity guard | `layers.rs:433` |
| §3.2 `layers`: omitted or `[]` is none, `"all"` is every reachable, an array is intersected | yes | `viewport_membership.rs:249`, `crates/tessera-server/tests/openapi.rs:819`, and the reserved-word refusal at `viewport_membership.rs:288` |
| §3.2 `levels`, `computed`, `artifact_rows` — defaults, narrowing, and the 422 vocabulary | yes | `layers.rs:790`, `:807`, `:831`, `:846`, `:903`, `:930`, `:947`, `:964`, `:1053`, `:1162` |
| §3.2 `artifact_budget` is honoured structurally, never by sampling | yes | `layers.rs:520` |
| §3.2 the *points* frame's `membership:<layer>` columns, by name, nullable, per served layer | yes | `viewport_membership.rs:168`, `:216` |
| §3.2 `underlay_offset` is refused, never clamped — three bounds | refusal yes (engine), **status mapping no** | `crates/tessera-engine/tests/viewport.rs:1578`, `:1592`, `:1608`; the 422 is **F3** |
| §3.2 `max_tiles_per_request` refuses rather than truncates | refusal yes (engine), **status mapping no** | `crates/tessera-engine/tests/viewport.rs:1647`; **F3** |
| §3.2 the four unconditional response headers, and `x-tessera-region` present exactly when asked | yes, driven off the OpenAPI description's own required/optional split | `openapi.rs:686`–`:734` |
| §3.2 `x-tessera-stale` is advisory and broadcast — geometry moved, not *your* geometry | yes | `http_engine_state.rs:77` (a superseded stamp) and `:135` (an overlay swap does **not** stale a geometry stamp) |
| §3.2 the two view coordinates are distinct, stable, and per principal | yes | `http.rs:94`, with the cross-principal `assert_ne!` that a deployment-key mix-up would trip |
| §3.2 `/v1/items` 404s identically for unknown and invisible | yes, byte for byte | `http.rs:323` |
| §3.2 `/v1/items` refuses a stale `idset` with 409 before inversion | yes | `http.rs:613`, `crates/tessera-server/tests/openapi.rs:864` |
| §3.2 a category arrives at drill-down as its **key**, never its code | yes | `crates/tessera-server/tests/openapi.rs:852` |
| §3.2 an unknown column is 422, an operator outside the family is 422, an unknown **value** is an empty operand | yes | `crates/tessera-server/src/filter_dto.rs:816` (`an_unknown_value_is_an_empty_operand_not_a_refusal`), `:830` (`an_unknown_column_is_refused`), `:900` (`an_operator_outside_the_family_is_refused`) |
| §3.2 every operator family's shape, including `range`'s two-bounds-one-side 422 and the empty range | yes | `crates/tessera-server/src/filter_dto.rs:910`, `:933`, `:942`, `:962`, `:974`, `:990`, `:1019`, `:1031` |
| §3.2 the `region` leaf: four shapes, `space`, by-artifact, the vertex cap, composition and negation | yes | `crates/tessera-server/src/filter_dto.rs:652`–`:815`, and end to end at `crates/tessera-server/tests/openapi.rs:713` |
| §3.2 `none_of`'s one-column rule and `none_of: []` | not here, deliberately | the parser defers it and says so at `crates/tessera-server/src/filter_dto.rs:858`; the rule is `tessera-filter`'s (track R5) |
| §3.2 nesting is bounded and refused rather than flattened | not here | `MAX_FILTER_DEPTH` is enforced in `crates/tessera-engine/src/filter.rs:2088` and `:2124` (track R1) |
| §3.3 `authorise` passes the decoded bytes verbatim; a zero-term credential mints a zero-visibility token | yes | `openapi.rs:459`, and the zero-term principal is used as a fixture at `crates/tessera-server/tests/openapi.rs:859` |
| §3.3 `revoke` is by `token_id` and takes effect without waiting for a sweep | yes | `http_engine_state.rs:229`, `:584`, `:654` |
| §3.4 ingest: either float width, the projected `lon`/`lat` spelling, clipping counted not refused | yes | `http_write.rs:2121`, `projected_ingest.rs:286`, `:354`, `:408`, `:436` |
| §3.4 ingest: duplicate external ids are 409 and the batch has **no effect** | yes, with `entity_id_high_water` read back | `http_write.rs:257`, `:306`, `:363`, `:406` |
| §3.4 ingest: a deleted holder does not collide, a suppressed one does | yes | `http_write.rs:4320` |
| §3.4 ingest: category keys not codes, an unknown key 422 whole-batch, null absent, empty string 422 | yes | `crates/tessera-server/src/control.rs:3431`–`:3560`, eight cases |
| §3.4 ingest: a layer-named column is that point's artifacts, with `value_set` deciding an unknown key | yes, at length | `crates/tessera-server/tests/membership_column.rs:782`–`:1391`, including `assert_same_database` against the build path |
| §3.4 changes: exactly one address form, a stale idset 409 before inversion, an out-of-range id 404 whole-batch | yes | `changes_addressing.rs:179`, `:219`, `:253` |
| §3.4 changes: the `predicate` op is withdrawn with a 422 naming the flow | yes | `http_write.rs:4279` |
| §3.4 `/control/flush` and `/control/compact` are 202 and deferred, one fold in flight | yes | `http_write.rs:2183`, `:2240` |
| §3.4 `/control/status` publishes the per-partition block and tier-scope `fragmentation` | yes | `http_write.rs:3837`, `:3949` |
| §3.1 the viewer plane's routes each require a session credential | **three of five, none enumerated** | **F1** |

Two rows outside this crate's ownership were not chased: `contracts §3.2`'s `POST /v1/region` is
marked ⊘ *specified, not built*, so there is nothing to test; and the `x-tessera-api` header the
same section required "everywhere" carries its own ⊘ saying no route emits or validates it.

## What was checked and found sound

Kept so the attacks are not re-run.

- **The control plane's two authentication tests are the model, and they document their own
  limits.** `every_control_route_requires_the_operator_credential` asserts the 401's body field by
  field — `error`, `detail`, and the *absence* of `retry_after_s` — because `contracts §3.1`'s code
  list is closed and a layer inventing its own body would be a wire change dressed as a refactor.
  Its sibling `every_path_on_the_control_listener_needs_the_credential` probes unrouted paths and
  near-miss spellings of `/healthz`, and states that the near-misses are there not because matching
  is a risk but because they are the exact strings a reintroduced exemption would be written
  against. Both carry a **Mutations this kills** paragraph.
- **`check_bearer` is tested against the attacks a naive compare loses to.**
  `crates/tessera-server/tests/http_engine_state.rs:390` covers prefixes, extensions and the empty
  string; the implementation hashes then compares in constant time, which the red-team confirmed
  has no prefix oracle.
- **An unauthenticated caller never reaches a body-shaped refusal.**
  `http_write.rs:3017` (`backpressure_is_invisible_before_auth`) and `:3571` (which reads the raw
  `HTTP/1.1 401` status line off the socket) pin that an oversized body, a missing batch id, a
  queue-full 429 and a row-cap 422 are all **401** without a credential — five distinct signals
  collapsed to one, across two servers configured to produce different halves.
- **The `k = 0` and `layers` defaults are tested against the *ruled* semantics and not the
  implemented ones**, which is the direction that catches a regression rather than blessing one.
- **The frame grammar is enforced by the decoder rather than per test**, so a response with two
  trailers, a sub-cells frame out of position, two artifacts frames or an unknown kind fails every
  test that decodes it — roughly forty cases — rather than the one that thought to look.
- **The artifacts frame's absences are asserted as absences.** `layers.rs:433` is written as *"the
  wire's headline, and the field it must not carry"*, and the engine's `ArtifactOut` doc names the
  three quantities that are deliberately not there (no ordinal, no declared size, no membership)
  with the C-row each would be.
- **The shape columns' conditional presence is distinguished from a null.**
  `crates/tessera-server/tests/common/mod.rs:827`–`:833` asserts that when the trailing pair is present it is present as a pair
  at columns 14 and 15 by name, and the decoder's three-level downcast fails loudly against a
  reader written for the old two-level hull — which is exactly the reader that would otherwise draw
  a second part as a hole of the first.
- **CORS is tested per plane and per list, including the transposition the struct exists to
  prevent.** `crates/tessera-server/tests/cors.rs` covers dev-only, production-only, both, and the
  control plane carrying no layer at all (`:374`), with the named `CorsOrigins` struct in the
  harness precisely because two adjacent `Vec<String>` parameters are what a caller transposes and
  transposing these is the bug [decision 0102](../../decisions/0102-the-viewer-plane-gains-an-enumerated-cors-origin-list.md)
  exists to prevent.
- **The fault switchboard's arming surface is bearer-gated and its sites are named**
  (`faults_surface.rs:155`), with an unknown site a 422 rather than a silent no-op — the right
  direction for a surface that exists only in a non-default build.
- **`/readyz` is tested from four independent causes** — a stepped-down partition, an engine with no
  write executor, a poisoned WAL that still accepts a deny, and the healthy control — rather than
  from one, so a readiness predicate that collapsed to a constant would go red.
- **The openapi binary keeps the description honest in both directions**: every route on the two
  planes and no other (`:346`), every closed DTO declared closed (`:375`), and every 429 in the
  document declaring `Retry-After` required (`:415`), then drives real requests against the schemas.
- **The build tool's identity refusals are covered by kind and by message**, including that a
  rotation refusal prints fingerprints and not keys — a disclosure assertion in a place a build
  tool's tests would not normally reach.

## Two things this track did not settle

Neither is a finding; both are recorded so the next reader does not spend the budget again.

- **`x-tessera-identity-key` collides by name with the deployment's identity key, and only one
  assertion separates them.** The header carries `ViewCoordinates::identity_key`
  (`crates/tessera-engine/src/viewport.rs:762`) — a 16-byte hash over the idset, the auth-data
  hash, the mask fragment's identity and the view — while `IdentityKey` is the blinding
  permutation's key, also 16 bytes, also rendered as 32 hex characters, and the test fixture's
  value (`TEST_KEY_HEX`) is exactly that shape. `viewport_serves_an_etag_and_an_identity_key_that_are_stable_across_requests`
  (`crates/tessera-server/tests/http.rs:94`) never asserts the header is not the deployment key
  directly; what saves it is the cross-principal `assert_ne!` at `crates/tessera-server/tests/http.rs:165`, since the deployment key
  would be identical for both principals. The test discriminates, so this is not a finding — but
  the assertion that carries the disclosure is not the one a reader would expect to, and the naming
  is what makes that so.
- **F1's absence is established for `tessera-server` and `tessera-cli` only.** The full workspace
  was deliberately not run — another audit track and a gate run were active — so no test anywhere
  posts an unauthenticated `/v1/items` is a claim about these two crates, which are the only ones
  that mount an HTTP listener.
