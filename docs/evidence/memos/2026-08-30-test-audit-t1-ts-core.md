# Test audit T1 — `clients/ts/core`

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests, not
the code under them**: nothing below is a claim that shipped behaviour is wrong, and no defect in
shipped code was found. Track T1 of Wave 3 of the test-quality campaign
([`test-audit-campaign.md`](../../test-audit-campaign.md)). The governing documents are
[`client-obligations.md`](../../design/client-obligations.md) and
[`client-interaction.md`](../../design/client-interaction.md); `conformance.md` is untouched.

## Results

**319 tests assessed across 28 files, and the assertions themselves are strong.** `npx vitest run`
in `clients/ts/core` at the base commit collects **319** cases in 28 files: **313 pass, 6 are
skipped** — the six being `test/client.live.test.ts`, gated on `TESSERA_LIVE=1` because it needs a
running server, with the reason stated in its own module doc. Nothing else is skipped, no
`it.skip`/`test.todo` exists anywhere, no snapshot assertion exists, no `.rejects` is left
unawaited, and there is no bare `expect(x);`. Where a test names a mechanism it generally pins it
hard, and two pieces of work are better than the surface average: `test/budget.test.ts` computes
the marks a request really costs from a **generative model independent of the code under test**,
and `test/frame.test.ts` plus the streaming tests pin the wire framing against golden captures.

**The three-defects question, answered directly.** `CLAUDE.md` records that one rename shipped
three defects into `clients/` — two app-state fields collapsed onto one name, a `.slice()` on a
typed array renamed to `.view()`, and a shadowed `const` used before its initialiser — and that
`scripts/check-clients.sh` exists because of them. **For `clients/ts/core/src/` the answer is yes,
decisively — and by the typecheck, not by the tests.** Planted in the worktree:
`result.ids.view(...)` at `src/bands.ts:307` is `TS2339`; a shadowed `const` appended to
`src/budget.ts` is `TS2448` *and* `TS2454`; and the state shapes are declared types
(`src/types.ts`, `store.ts`'s projections) with no `any` anywhere in `src/`, so a field collapse is
a type error at every remaining reader. `npm run typecheck` in core is clean today and reports all
three classes.

**For `clients/ts/core/test/` the answer is no, and that is finding F1.** `core/tsconfig.json`
includes `src` only, and vitest strips types without checking them, so **nothing in the gate
typechecks 6,379 lines of test code**. Planting *both* rename defects in
`clients/ts/core/test/decode.test.ts` leaves `tsc -p tsconfig.json --noEmit` at exit 0 and the
suite green. Core is the only workspace
with test files where this hole exists: `deck`, `components`, `react` and `wire-example` all name
`test` in their `include`. 42 type errors have already accumulated behind it, including one
genuinely vacuous assertion.

**Seven findings.** Five were confirmed by mutation in a throwaway worktree; the whole suite ran
under each and the counts are quoted. None is S1: the disclosure surface a client owns is
`client-obligations.md`'s own "what this page does not say" — a client cannot add to the leak
register, only misrepresent a true answer — and I have reserved S1 for a breach that would misstate
*what a principal may see* (rules 1, 2, 4, 5 of the twelve). Every one of those is covered, most of
them well. What is missing sits on the rules about *reaching* an answer at all.

**Attacks that failed, kept so they are not re-run.** Frame-length endianness is pinned hard, not
by construction: `getUint32(1, true)` → `getUint32(1, false)` in `src/frame.ts:94` turns **57
tests red across 7 files**, because the fixture-based tests walk real server captures. Lowering
`MIN_DEPTH` from 3 to 0 in `src/budget.ts:40` is caught. `composeFilters` returning `{all_of: []}`
for an empty draft, and `isPopulated` forced to `true`, each turn two store tests red — so
`src/filters.ts`, which no test imports directly, is covered through `store.setFilters`.

## Findings

### F1 — nothing in the gate typechecks `clients/ts/core/test/`

**Claim:** the TypeScript client is in the gate so that a rename cannot ship a defect unnoticed
(`scripts/check-clients.sh`'s own header). That protection stops at `src/`.

**Evidence.** `clients/ts/core/tsconfig.json:3`:

> `"include": ["src"],`

and `package.json`'s `typecheck` is `tsc -p tsconfig.json --noEmit`, so the test directory is
outside the program. Vitest transpiles with esbuild and performs no type checking. Running `tsc`
over `src` **and** `test` together reports **42 errors in 13 of the 28 test files** and **zero in
`src`**. Three are the rename classes themselves rather than fixture drift:

- `test/artifacts.client.test.ts:17` uses a free identifier — `kind === FRAME_ARTIFACTS`, `TS2304`,
  nothing imports it. Its enclosing helper `reframe` is never called, so the `ReferenceError` it
  would throw is latent rather than live.
- `test/bands.test.ts:20` sets `codes` on a `Band`, `TS2353`; `Band` has no such field
  (`src/bands.ts:54`–`:101`). The fixture computes a `BigUint64Array` of Morton prefixes per band
  and hands it to a type that dropped the property.
- `test/client.live.test.ts:146` compares a property neither type has — see **F7**.

The remaining 39 are fixtures that no longer satisfy the wire types:
`ViewportResult.artifactsIdentity` missing from every fixture in `bands.test.ts` and
`driver.test.ts`, `ViewportResponse.region` missing from `driver.test.ts` and
`artifactChannel.test.ts`, and `ViewInfo`'s four newer fields missing from `extent.test.ts`.

**Mutation run.** In a throwaway worktree, both rename defects were appended to
`clients/ts/core/test/decode.test.ts` — `a.view(0)` on a `BigUint64Array`, and a `const` read
before its declaration. `npx tsc -p tsconfig.json --noEmit` exited **0**; the file's own
`npx vitest run` was **12 passed**. Planted in `src/` instead, the same two lines produce
`TS2339`, `TS2448` and `TS2454`. Reverted; nothing committed.

**Class:** unreachable — the check exists and does not run over these files. **Severity: S3** — a
real defect could pass, and one already has (F7).

**What a defect would let through:** a fixture that silently stops modelling the response it claims
to model, and any assertion whose subject the production type has removed, which reads
`undefined === undefined` and passes. This is the exact mechanism `check-clients.sh` was written to
close, on the half of the workspace it does not cover.

**Confidence:** high, mutation-proven in both directions.

**Disposition:**

### F2 — no test asserts the URL either by-id route fetches, so rule 9's decimal string is unpinned

**Claim:** `client-obligations.md` rule 9 — a `tessera_id` is a `u64`, carried as a decimal string
in the path segment of `/v1/items/{tessera_id}` and `/v1/artifacts/{tessera_id}`, **never a JS
`number`**, which loses bits past 2⁵³; the golden fixture's first id already does.

**Evidence.** Both call sites are correct today:

> `const response = await fetch(\`${this.opts.viewerUrl}/v1/items/${tesseraId.toString()}\`, {`
> — `clients/ts/core/src/client.ts:620`, and `:657` for `/v1/artifacts/`

`test/artifacts.client.test.ts` has five cases against `client.artifact(...)` (`:274`–`:329`), each
with a stubbed `fetch` that **ignores the URL it was given** and answers from a closure. Nothing
anywhere reads the request URL. `client.item(...)` has no test outside the skipped live file at
all (see F3). The one place the id width *is* pinned is the decode side —
`clients/ts/core/test/decode.test.ts:82`, `expect(r.ids).toBeInstanceOf(BigUint64Array)` — which
covers the wire but not the two routes that spend the id.

**Mutation run.** `tesseraId.toString()` → `Number(tesseraId)`, separately at `:620` and at `:657`.
Both runs: **313 passed, 6 skipped, 0 failed** — the full baseline. Reverted; nothing committed.

**Class:** under-discriminating for `/v1/artifacts/` (tests exist and cannot see it), missing for
`/v1/items/`. **Severity: S3.**

**What a defect would let through:** exactly what rule 9 describes — above 2⁵³ the id rounds, and
the pick returns `404 unknown` for an item on screen, or the record of a **different** item, with
nothing saying why. Not S1: the server still gates by the id it receives, so the failure
misrepresents which item was opened rather than widening what the principal may see.

**Confidence:** high, mutation-proven twice.

**Disposition:**

### F3 — three of `TesseraClient`'s six verbs have no test that runs in the gate

**Claim:** `client.ts` is the whole of this client's contact with the wire. `authorise`
(`src/client.ts:154`), `item` (`:619`) and `categories` (`:563`) have real logic and are exercised
only by `test/client.live.test.ts`, which is skipped unless `TESSERA_LIVE=1`.

**Evidence.** `viewport` is covered by `test/artifacts.client.test.ts` and
`test/stream.client.test.ts`, `meta` by `test/artifacts.client.test.ts:127`, and `artifact` by five
cases at `:274`–`:329`. For the other three, `test/store.test.ts:154` replaces the entire client
with a fake — `item`, `categories` and the rest are closures in the test file — so the store tests
prove nothing about them. Two behaviours with a stated consequence are unexercised:

- `categories`' early return for an empty code list, whose own comment says why it is there —
  *"keeps an empty request from being read as the enumeration form, which would fetch the whole
  vocabulary"* — and the paging loop `do { … } while (cursor !== null)` beside it.
- `authorise`'s `btoa(JSON.stringify({terms}))` encoding of the auth data, which is the request
  shape the session plane parses.

**Mutation run.** Deleting `if (opts.codes.length === 0) return out;` from `categories`: **313
passed, 0 failed.** Reverted; nothing committed.

**Class:** missing. **Severity: S3.**

**What a defect would let through:** a filter panel that enumerates a 60,000-value vocabulary on
every ask, a paging loop that does not terminate, or a malformed authorisation body — each visible
only against a live server, which is the configuration CI does not run.

**Confidence:** high; established by reading which files import `client.ts` and confirmed by
mutation for `categories`.

**Disposition:**

### F4 — the worker decode path has no test, and its injection seam is used by nothing

**Claim:** `src/decoder.ts` exists because decode is the client's throughput limit and was
competing with drawing — its module doc quotes 2.8 s of decode making a 9 ms pan take 8.8 s to
paint. `workerDecoder` (`src/decoder.ts:99`) and `src/decode.worker.ts` are the production path in
a browser. Neither has a test.

**Evidence.** `src/decoder.ts:100` is `if (typeof Worker === 'undefined') return null;`, and
`vitest.config.ts` sets `environment: 'node'`, where `Worker` is undefined — so **every test in
this suite takes `inlineDecoder`**. No test file imports `decoder.js` or `decode.worker.js`;
`test/artifacts.client.test.ts:68` supplies its own two-method stub. `setWorkerFactory`
(`src/decoder.ts:80`) exists precisely as an injection seam for a bundler that cannot resolve the
worker URL, and **nothing in the repository calls it from a test**.

Three claims in that file are pure logic and would be testable through that seam: the two-lane
routing (*"a foreground response must never queue behind an anticipatory one … two workers, one per
lane, and the flag is the routing"*, `:26`–`:29`), the `onerror` handler that rejects every
outstanding promise so a dead worker does not hang its callers (`:126`), and the transfer list in
`decode.worker.ts`, where a buffer omitted is a silent copy and a buffer wrongly included is a
detached array on the next read.

**Class:** missing. **Severity: S3.**

**What a defect would let through:** `src/decode.worker.ts` could be emptied and this suite stays
green. A routing defect shows as the latency regression the file was written to fix; a transfer-list
defect shows as a detached `ArrayBuffer` and a blank map — in a browser only, which nothing in the
gate is.

**Confidence:** high, from reading; no mutation, because the absence is total and a mutation would
only restate it.

**Disposition:**

### F5 — obligation 11's `401`-ends-the-session rule has no test

**Claim:** `client-obligations.md` rule 11 — a `401` on a token that previously worked is the
session ending, the same as a `403`; a client that reads it as *fix your credentials* stalls on a
revoked session. `src/store.ts:439` implements exactly that, with a condition subtle enough to
deserve one:

> `return refusal.code === 'bad-credential' && tokenEverUsed;`

**Evidence.** `store.get('status').expired` is asserted **nowhere** in the suite. The only test
anywhere that mentions either code is `test/presented.test.ts:186`–`:190`, which throws a
`TesseraError(403, 'expired-token', …)` to check that the refusal *detail* reaches `onStatus` — it
asserts the presenter's passthrough, not `isExpiry`, and the store is not in that test at all. The
`tokenEverUsed` distinction the comment draws (*"before the store has used a token, it is a bad
option, not an expiry"*) has neither arm covered.

**Mutation run.** `src/store.ts:439` replaced with `return false;`, which is a client that treats
every `401` as a bad credential rather than a swept session. **313 passed, 6 skipped, 0 failed.**
Reverted; nothing committed.

**Class:** missing. **Severity: S3** — not S1, because the breach stalls a session rather than
misstating what a principal may see.

**What a defect would let through:** the exact behaviour rule 11 names — a revoked session that
never re-authorises, retrying a request that will never succeed.

**Confidence:** high, mutation-proven.

**Disposition:**

### F6 — the `empty` display state is never asserted, so collapsing it into `shown` passes

**Claim:** `client-obligations.md` rule 1 — the six display states are distinct and **only `shown`
carries a number**; `empty` is a real answer of zero and a number rendered under any other state is
a number of nothing.

**Evidence.** `'empty'` is emitted from exactly one place:

> `this.events.onStatus?.(actual === 0 && visible === 0 ? 'empty' : 'shown');`
> — `clients/ts/core/src/driver.ts:653`

It flows through `Presenter.transition` (`src/presented.ts:250`) into the store's `status`
projection (`src/store.ts:554`). No test in the suite asserts the string `'empty'`; the only status
assertions are `'shown'`, `'refused'` and `'retrying'`. The dangerous direction — a refusal drawn as
an empty map — *is* covered, at `test/presented.test.ts:181`–`:190` and
`test/artifactChannel.test.ts:127` (*"a refusal is not an empty view"*), which is why this is the
lesser half of the rule and graded accordingly.

**Mutation run.** `src/driver.ts:653` reduced to `this.events.onStatus?.('shown');`. **313 passed,
6 skipped, 0 failed.** Reverted; nothing committed.

**Class:** missing. **Severity: S4** — weak but not misleading: the collapse is `empty` → `shown`,
which renders a zero rather than an unknown.

**What a defect would let through:** a view of a real zero presented as `shown`, which under rule 1
is the state that carries a number, so a panel would draw *0* where the design wants the empty
state. Note also that `status.sessionWarm` is set from `status === 'shown'` (`src/store.ts:556`), so
the two states are not interchangeable downstream either.

**Confidence:** high, mutation-proven.

**Disposition:**

### F7 — `client.live.test.ts:146` compares a property neither type has

**Claim:** the live test asserts that the drill-down and the map agree — *"an artifact openable but
not drawable, or the reverse, would be that rule transcribed twice"*.

**Evidence.** `clients/ts/core/test/client.live.test.ts:146`:

> `expect(opened.stableKey).toBe(first.stableKey);`

`stableKey` exists on neither `ArtifactDetail` nor `Artifact` (`TS2339`, twice, on one line). At
runtime both sides are `undefined`, so the assertion is `undefined === undefined` and cannot fail.
The two assertions bracketing it — `opened.layer` against `first.layer`, `opened.maskedCount`
against `first.maskedCount` — are sound, so the case still checks two of the three properties it
names; what has been lost is the key, which is the identifier half of the agreement.

This is the only vacuous assertion found on the surface, and it is direct evidence for **F1**: it
is a rename that removed a field, and the one check that would have caught it does not read this
file. It is ranked last because the file is also skipped in the gate, so its practical weight today
is nil — but a track running `TESSERA_LIVE=1` would read this case as covering more than it does.

**Class:** vacuous. **Severity: S4.**

**What a defect would let through:** a client whose drill-down and viewport disagree about an
artifact's key, in the one test written to catch that.

**Confidence:** high, from the compiler and from reading; no mutation, because a tautology needs
none.

**Disposition:**

## L4 — the twelve obligations, one row each

Built before reading the tests, per the campaign's own lesson. The third column is the test that
would go red if the rule were violated.

| Rule (`client-obligations.md`) | Covered? | Where |
|---|---|---|
| 1 — display states distinct; only `shown` carries a number | refused/empty arm yes, empty/shown arm **no** | `test/presented.test.ts:181` and `test/artifactChannel.test.ts:127` for the arm that matters; **F6** for the other |
| 2 — both figures of a sample, or neither | yes, every branch | `test/counts.test.ts:13`–`:28`, including the inexact and stale cases |
| 3 — a stale view is marked, keyed on the content key and never on `x-tessera-stale` | yes, end to end | `test/store.test.ts:221`–`:255` drives a real revalidation that observes `ck-2`, marks stale, and clears on `refresh()`; `test/replica.test.ts:169` covers the replica half |
| 4 — a masked count is one figure with no denominator | yes | `test/counts.test.ts:33`–`:45`; the `Masked` type carries no second field |
| 5 — an absent artifact has no reason; an absent value is an empty answer | yes | `test/frame.test.ts:31` (absent, not empty, on two golden captures), `test/artifacts.client.test.ts:238`, `test/artifacts-frame.test.ts:220` |
| 6 — the artifact channel asks for itself: `layers: [...]`, `k = 0` | yes | `test/artifactChannel.test.ts:115`; `test/artifacts.client.test.ts:74` and `:98` pin the wire names, `test/extent.test.ts:137` separates the two request kinds |
| 7 — held state drops on an identity-key or filter change; the payload is the exception | yes, all three | `test/replica.test.ts:243` (identity rotation), `test/artifactChannel.test.ts:206` (both keys), `test/store.test.ts:286` (filters), `test/artifactChannel.test.ts:232` (the bit is taken from the response, never held) |
| 8 — `k` never decreases on zoom | **no test**, holds by construction | `k` is `meta.kMaxMarks` at all eight call sites in `src/driver.ts`; nothing varies it, so there is no defect a test could presently distinguish. Recorded, not reported as a finding |
| 9 — a `tessera_id` is a `u64`, decimal on the path, never a JS `number` | wire half yes, path half **no** | `clients/ts/core/test/decode.test.ts:82` for the decode; **F2** for the two by-id routes |
| 10 — six headers survive a proxy | two of six | `test/stream.client.test.ts:78` asserts `etag` (quote-stripped) and `x-tessera-identity-key`; `x-tessera-pin` is deliberately unused by a client (`client-interaction.md` §4, *"the geometry stamp is invisible to a client"*, decision 0041), and the two timing headers are instrumentation. Not a finding: the obligation is the proxy's |
| 11 — a `401` on a used token is the session ending | **no** | **F5** |
| 12 — depth is the client's choice: floor, cap, saturation, damping, motion rule | yes, every clause, unusually well | `test/budget.test.ts` — floor at `:45`, cap at `:51`, saturation at `:56` and `:172`, damping and both clamps at `:229`–`:266`, the anti-oscillation property at `:268`; the count-driven model is checked against a generative `truth(depth)` the code under test never sees |

## What I checked and found sound

- **The wire framing is pinned by assertion, not by construction.** The brief asked specifically:
  the Rust side had no byte-level test of `contracts §3.2`'s `u8 kind ‖ u32 LE length` until this
  campaign, and the TS reader decodes little-endian independently at `src/frame.ts:94`. Reading it
  little-endian in the *synthetic* tests would indeed agree by construction — the helper at
  `test/frame.test.ts:49` writes with `setUint32(…, true)` — but four of the nine cases walk
  **golden captures from a real server**, and `getUint32(1, false)` turns **57 tests red across 7
  files**. The grammar itself is covered arm by arm: a second artifacts frame, a misplaced one, a
  truncated body, a missing trailer, an unknown kind, and an exact whole-payload accounting.
- **`FrameReader` is proven to be one reader.** `src/frame.ts`'s doc says two readers would be two
  chances to disagree; `test/stream.client.test.ts:142` feeds the same body at chunk sizes 1, 7, 64,
  1,000 and whole, and asserts the parts' tiles and points equal the whole-body decode exactly —
  including a size that splits every five-byte header across three chunks.
- **The budget is the strongest work on this surface.** `test/budget.test.ts` builds an independent
  generative field and asserts on `truth(depth)` — *what the server would serve at the chosen
  depth, not what was predicted for it* — which is what makes the bimodal-overshoot case a real
  measurement rather than a restatement.
- **I10 is not a client question, and the tests are right not to invent one.** Nothing in
  `clients/ts/core` evaluates a visibility rule, derives a mask, or reconstructs an entity id;
  identities cross as opaque `BigUint64Array` values and every count arrives computed. The one
  place a client could invent a distinction the server refuses to make is the drill-down's 404,
  and `test/artifacts.client.test.ts:320` exists for it — see the caveat below.
- **Cache and coverage discipline is covered from both directions.** `test/replica.test.ts` pins
  that a covered region is never re-asked (so empty ground does not spin), that coverage bought at a
  smaller `k` is not reused, that a large region splits, and that a bypassed cache retains nothing
  and costs `bytes === 0`.
- **The refusal-vs-empty opposition is covered where it counts.** `test/artifactChannel.test.ts:127`
  and `test/presented.test.ts:181` both assert the refused path clears the drawn set and reports a
  code, and `src/store.ts:567` extends it to the region projection with a test at
  `test/store.test.ts:533`.
- **`src/filters.ts` imports no test file and is nonetheless covered.** Two mutations confirm it —
  see *attacks that failed*, above. What is genuinely unexercised is `emptyDraft`'s operator-set
  skipping and `operatorOf`'s text and numeric arms, all of which produce a request body; that is
  thin rather than absent, and it is not reported as a finding.

**One caveat inside the sound column, which did not reach a finding.**
`test/artifacts.client.test.ts:320`, *"surfaces the one refusal as a typed error, with nothing else
to read from it"*, asserts only `rejects.toThrow(TesseraError)` — the JS analogue of the bare
`assert!(x.is_err())` this campaign catalogued. Neither the `404`, nor the code, nor the detail is
checked, so the second half of the name is unasserted. It is not reported because the property it
guards is the *server's* (every withheld case answering identically) and lives in the engine's
tests; on the client side there is nothing for a defect to widen. Worth a sentence in the test's own
doc rather than a fix.

## Two things this track did not settle

- **The 42 type errors were not triaged one by one.** Three are named in F1 because they are the
  rename classes; the other 39 are fixture drift against `ViewportResult`, `ViewportResponse` and
  `ViewInfo`. Whether any of them makes an assertion weaker was not checked exhaustively —
  a fixture missing a field the code under test reads would normally fail loudly, and none does
  today. Fixing `tsconfig.json` will surface all 42 at once and that is where they should be read.
- **Whether the same hole exists in `viewer`, `harness` and `spike`** — `viewer` and `spike` also
  include `src` only, but neither declares a `test` script, and `harness` typechecks its `.mjs`
  under `checkJs`. That is track T2's surface and was not audited here.
