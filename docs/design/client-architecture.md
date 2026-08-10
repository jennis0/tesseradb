# Client architecture: tessera-client and tessera-vis

**Status:** Provisional — awaiting adversarial review and owner sign-off on the decisions in §7.
Supersedes the client-side layering of the 2026-08 client plan where the two differ; defers to
`client-interaction.md` for every obligation it restates.

## 1. The boundary, and the rule that draws it

Two packages, split by one rule:

> **If a behaviour affects what is asked of the server, or what may be presented as data —
> counts, provenance, which subset of a tile is drawn — it belongs to `tessera-client`. If it
> affects only how pixels are produced, it belongs to `tessera-vis`.**

**`tessera-client`** is the SDK every consumer ships, including consumers building their own
visualisations. It must be *complete*: a dependent client that uses it and draws what it is
handed is conformant without reimplementing anything conformance-bearing. Headless by
construction — no DOM, no deck.gl, no ambient timers; the clock and the fetch are injected, so
every behaviour in it is testable without a browser. It contains:

- the **session client** (transport, decode workers, coordinates) — unchanged;
- the **replica** (bands, coverage, eviction, the emptiness cache) — API reshaped, §4;
- the **driver** — the scheduler that today lives, dissolved into timers, inside the viewer
  (§3): gesture state, request pipelining, retry and 429 posture, the staleness cadence,
  anticipation pacing, depth-hold release;
- **policy oracles** (`prefetch.ts`, `budget.ts`) — pure, unchanged in role;
- **frame composition** (§5): the invariant-bearing parts of today's `assemble.ts` — the
  id-order-prefix rule, per-tile density matching, exact-supersession, provenance and the
  no-counts-on-non-exact rule — as a pure computation from replica state to a typed draw list.

**`tessera-vis`** (today `@tessera/viewer`) is the end-to-end display: the deck.gl binding and
its GPU slab (owned buffers, dirty spans, partition retention), colour encoding, panels, the
trace bar, DOM wiring. It holds no timers except `requestAnimationFrame` coalescing, makes no
requests, and decides nothing about data: it renders draw lists and forwards view events.

What moves where, concretely:

| today | lands in |
|---|---|
| `ViewportController` timers, pipelining, retry, settle, anticipation, budgets | client (driver) |
| `assemble.ts` density matching, fold filter, provenance/tiles, fidelity assertions | client (composition) |
| `assemble.ts` buffer concatenation; colour attribute building | vis |
| `slab.ts`, `binary`/`gpuAttributes`, layer construction | vis (unchanged) |
| rAF redraw coalescing, panels, legend timing | vis (unchanged) |
| `redrawFromCache`/`request`/margin-arm reconciliation (four copies) | client (driver), once — against the presented-frame handle, §3 |
| item-detail fetch on pick; category-code resolution (both issue requests today from `main.ts`) | client; vis renders the results |
| legend code-counting (folds over held bands) | client (composition); vis renders |
| colour encoding accumulators (ranks, domains) — computed from data, for pixels only | vis — but their **session-scoped lifetime is client-enforced**: reset on the replica's identity-key change, so cross-principal persistence cannot depend on viewer discipline |

## 2. Why this is a redesign and not a move

The 2026-08 plan assigned scheduling to the library ("cache is what answers, prefetch is what
asks"). What was built instead left a pure geometry oracle in core and grew the actual asking
machinery inside the deck binding, one timer per fix. The 2026-08-10 architecture review found
the result structurally wrong (nine findings; see Appendix D for the trail), and Phase-0
measurement found it **behaviourally** wrong: the composed scheduler had drifted from its
design with nothing able to see the drift. The measured divergence (Appendix M) is the
specification baseline for §3 — the driver implements the *designed* behaviours, not the
current accidental ones.

## 3. The driver

One object in `tessera-client`, constructed with `{replica, clock, now}` injected. It is an
explicit state machine; every timer the controller accreted becomes a transition with a
per-state timeout, and every request — including retries — passes through one in-flight
bookkeeping.

The machine is **four orthogonal regions**, not five exclusive states — the measured common
case runs several at once (a pipelined pan is motion *and* a request in flight; the settle
timer re-arms across a stream of arrivals; an in-flight ring deliberately survives movement,
its server cost already paid):

- **Motion** — `still | gesture`. Owns the debounce tiers, the leading edge and its wall-clock
  floor, derive gating and depth-hold. Gesture end arms the settle region. Inputs include
  **page visibility, injected** like the clock — the driver is headless, so DOM state arrives
  as an input, never a read.
- **Foreground lifecycle** — `idle | primary | margin`, with the queued-view slot and
  generation/abort discipline owned here, and the **margin leg an explicit phase**: a second
  request after the primary arrival, superseded by a queued view exactly as the primary is.
  A retry is a timed re-entry into `primary` inside the same generation discipline, cancelled
  by the same `cancel()` as everything else (today it is an untracked `setTimeout` that
  survives principal switches). The counts-only revalidation request runs in this region's
  slot — never alongside a real fetch, aborted by movement like any request.
- **Settle cadence** — `idle | armed(deadline)`. Owns the settle timer, `SETTLE_MAX_MS`, and
  depth-hold release.
- **Anticipation** — `idle | eligible | bite-in-flight`. Entered from stillness after the idle
  delay; a bite already in flight survives motion (the cost is paid), but *eligible* is
  cancelled by it. The measured fix for the dead ring: deferral is a *wait*, not a discard —
  eligibility re-evaluates when the deferring condition clears, guarded by **no foreground in
  flight and no queued view pending** (a chained dispatch keeps deferring it, so a bite never
  queues ahead of the user at the admission gate), without re-applying the idle delay within
  one pause. The bites-per-pause and byte budgets then bind for the first time; movement still
  cancels pending eligibility and resets them.
- **Staleness cadence** — owned by the driver's clock against the *presented* view: while
  still and visible, past `revalidateAfterMs`, issue the counts-only refresh through the
  foreground region. Whether this cadence exists at all is D2 — §7 states the zero-cost
  alternative.

Scheduler-facing state moves with the machine: `mTarget` and the calibration observation,
`lastVisibleInView`, and status are driver state, not viewer store state — they set what is
asked of the server. The driver also tracks the **presented-frame handle** — want rectangle,
depth, store version, stand-in staleness — which is definable in `tessera-client` from the
first migration step, before composition itself moves (§6): the redraw reconciliation reads
only this handle, never a viewer object.

The driver's outputs are values and callbacks — `onFrame(draw list)`, `onStatus`, trace hooks
(the events §6's acceptance measures are emitted here) — never DOM effects. Its unit tests are
the Appendix M rows driven by a fake clock: the three live defects (starved revalidation, dead
budgets, orphan retry) are written as failing tests against it before migration begins, and go
green as behaviours move.

**The driver is the default scheduler, not the only path.** It encodes one interaction model —
continuous direct-manipulation pan/zoom — and its pacing (debounce tiers, idle thresholds,
velocity from view-state deltas) is tuned to that model on one frontend and one input-device
class: transfer to other interaction models is assumed, NOT measured. A frontend whose
viewport changes differently (programmatic fly-to, dashboard viewport swaps, keyboard jumps)
drives the replica, oracles and composition directly. The boundary that makes this safe is
which layer enforces what: **the replica and composition enforce the conformance-bearing rules
on every path** — replace-never-merge across a content-key change, id-order-prefix subsetting,
no counts on non-exact tiles, empty distinct from refused — while the driver *discharges* the
behavioural obligations (429 posture, the staleness cadence, anticipation pacing) on the
consumer's behalf. A consumer bypassing the driver inherits that second list as its own
obligations, and the package documentation states the two lists in exactly those terms.

## 4. The replica's shape

`fetchRegion`'s eight positionals mixing transport, piece pipelining, absorb, coverage,
revalidation and frame derivation split into what they always were:

- **`fetch(region, opts)`** — plan against coverage, pipeline centre-first pieces, absorb,
  mark coverage. Returns what was fetched and the honest plan summary (issued requests, total
  bytes). Options object; no booleans steering the return shape.
- **`read(region, depth, k)`** — today's `frameFromCache`: pure, from held state.
- Revalidation moves out entirely — the driver owns the cadence (§3); the replica keeps only
  `dueForRevalidation()` as a query.

The tile-addressed path (`tile()`, `scheduleFlush`) is **deleted** (D3, §7). The plan's Layer 1
was tile-shaped to serve a future `TileLayer`-style consumer; the built replica is
rectangle-shaped because tile-list enumeration costs what the plan's own arithmetic refused to
pay, and the review judged the deviation an improvement. The vestige as it stands violates the
empty-vs-refused rule on its error path and coalesces by bounding box; pre-release (0048),
deletion beats repair, and the supersession is recorded rather than implicit. A future
tile-shaped consumer wraps `fetch`/`read`.

## 5. Frame composition, unified on the tile grid

Three mechanisms currently enforce "a stand-in reads as the drawn depth and never
double-draws", at three granularities in two packages: rect-level coverage clipping in the
replica, per-tile density matching and per-mark fold filtering in the viewer. They become one
computation in `tessera-client`, answering for the drawn grid *which bands contribute, at what
prefix length, on what authority* — but evaluated **per contributing band projected onto the
integer grid, never per tile**: an ancestor projects as a clipped rectangle (one entry per
band, as today), and per-tile materialisation happens only for exact and descendant claims,
which are one tile each. A literal per-tile pass would re-buy the O(viewport-tiles) walk the
current code twice refused by measurement (181 ms to enumerate a 262k-tile region). The
two-tier evaluation also survives unification: the arrival fold stays cheap (exact claims and
supersession only), the full derivation runs at the settle — same computation, two trigger
points. Exact bands claim their tiles; descendants under an unclaimed tile share its density
target by largest remainder; ancestors clip to unclaimed ground. The output draw list carries per-tile provenance, so the no-counts rule and the
fidelity assertions range over one structure instead of three reconstructions.

Two constructions this hardens, per the review's Question E audit: the ascending-id premise
every prefix operation rests on gains its assertion at `bandSplitter` (wire order, checked
once, everything downstream is construction); and the fold filter's float-recovered tile
indices are replaced by the composition's own integer grid, closing the boundary-mark leak
`rects.ts` bans in its own domain.

`tessera-vis` consumes the draw list: concatenation, colours, slab writes. Nothing in it
subsets data again.

## 6. Migration, and what proves it

Strangler order, demo working at every step, sanity + traces + density audit as the net:

1. Driver lands in `tessera-client` with fake-clock tests red for the three live defects;
   request path (pipelining, retry, generation) migrates; tests green.
2. Anticipation and staleness cadence migrate; Appendix M re-measured — bites/pause and
   revalidation rows must move from *dead* to *designed*.
3. Composition unifies on the tile grid; density audit stays clean; ascending-id assertion in.
4. Replica API split lands; `tile()` deleted; viewer renamed `tessera-vis` (D4).
5. The controller's timer fields are deleted. What remains in vis is the §1 list, verified by
   the package having no timer other than rAF and no import of the driver's internals.

Acceptance: the frame targets (>45 fps, p95 <100 ms), first-paint p50 ≤350 ms / p95 ≤1 s on
novel ground, covered rate ≥90%, wholly-novel arrivals materially below the measured 31%,
revalidation observed at its cadence, density audit clean — all from one trace protocol run,
plus the driver's headless suite in CI.

## 7. Decisions for the owner

- **D1a — the boundary rule of §1** and the two-package split, with the driver as the default
  scheduler and the enforce-vs-discharge obligation split of §3.
- **D1b — RULED 2026-08-10: moot in private development.** Nothing is public yet; the driver
  develops freely as ordinary client API under 0048. The "stable client" bar attaches at
  public launch, not before.
- **D2 — RULED 2026-08-10: a few minutes of staleness is acceptable; latency-neutrality is
  the binding requirement.** Either form may be built (a timer, if built, is configurable);
  what is ruled is the constraint: the counts-only refresh must never add latency to the
  viewer — it runs only when the foreground slot is otherwise idle, is aborted by movement,
  and never displaces or delays a real fetch.
- **D3 — RULED 2026-08-10: delete the vestige; preserve the tile-engine story.** What the
  owner cares about is future support for **tile-based visualisation engines**, not tile
  addressing per se. `tile()`/`scheduleFlush` (verified consumers: two unit tests) are
  deleted; the supported integration path for a `TileLayer`-style engine is an **adapter over
  `fetch`/`read`** — per-tile asks batched into region fetches, each answered from `read`,
  with empty distinct from refused per ask — documented with §3's driver-bypass obligation
  list. The rectangle-shaped supersession of the plan's tile-shaped Layer 1 is recorded here.
- **D4 — RULED 2026-08-10: no rename.** `tessera-vis` was a stand-in name; the package stays
  `@tessera/viewer`. This document keeps "vis" as the boundary vocabulary only.
- **D5 — the anticipation spend the re-arm unlocks.** Fixing the dead ring moves measured
  anticipation from ~0.05 bites per pause toward the designed ≤3 — up to ~60× today's ring
  spend, the fleet cost the look-ahead probe priced at roughly half again the server CPU per
  pan for a doubled no-request rate. Bound: bites × byte budget × pause rate, all knobs.
  Ruling wanted on the default posture (ship at design budgets, or ramp behind measurement).

## Appendix M — measured divergence (2026-08-10, trace T22-03-36, 88 s, 10⁹ corpus)

| behaviour | designed | measured |
|---|---|---|
| ring bites per idle pause | ≤3, byte-budgeted | 2 rings / ~41 pauses (~0.05); budgets never bound |
| pause-loss cause | — | 27/39 skips: foreground in flight at idle-fire, no re-arm |
| pans needing no request | ~92% (look-ahead probe) | 86% |
| wholly-novel foreground viewports | rare | 8/26 arrivals (31%) |
| revalidation at rest | ≤60 s cadence | zero events in 88 s |
| frame targets | >45 fps, p95 <100 ms | met (50.3 fps) |
| density anomalies | none | 17, clustered at depth transitions |

## Appendix D — provenance

The 2026-08-10 adversarial architecture review (nine findings) is the structural evidence for
§§2–5; the mechanism ledger and stage cost model are in
`docs/evidence/memos/2026-08-10-viewer-pipeline-cost-model.md`. Phase-0 instrumentation
(`ring`/`ringskip`/`covered`/`novel`/`revalidate` trace events) landed in `affd5bc` and is what
produced Appendix M.

## Appendix R — review trail

- 2026-08-10: drafted (Provisional).
- 2026-08-10: adversarial review, eight findings, all accepted in one disposition pass —
  §3 rewritten from five exclusive states to orthogonal regions (F1); item/category requests
  and the legend fold routed to client, encoding accumulators to vis with client-enforced
  lifetime (F2, F8); §5 qualified per-band-projected, two-tier evaluation retained (F3);
  anticipation re-arm guard stated and its spend made D5 (F4); D1 split into D1a/D1b (F5);
  D2 restated with the interaction-driven alternative and visibility as an injected input
  (F6); the presented-frame handle added so migration step 1 does not read viewer state (F7).
  Reviewer verdict: ready to bind after these changes; D3's deletion verified safe.
- 2026-08-10: D4 ruled — no rename; the package stays `@tessera/viewer`.
- 2026-08-10: owner "generally on board" (D1a accepted); D1b ruled moot pre-launch; D2 ruled
  — minutes-scale staleness acceptable, latency-neutrality binding, timer (if any)
  configurable; D3 ruled — delete, with the tile-engine adapter story preserved over
  `fetch`/`read`.
- 2026-08-10: D5 ruled — **start at design budgets, then measure**: the restored ring ships
  at its designed bites/byte budgets, and the first Appendix M re-measure judges the spend.
- All decisions ruled. Phase 2 (§6 migration) begins.
