# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

A permission-masked point service: a pannable, zoomable map over a document corpus where what a
viewer may see determines not just which items they retrieve but **every count, density, cluster
and summary they are shown**. A viewer's visible set is materialised once per session as a Roaring
bitmap; geometry is stored in Morton order so a tile is a contiguous row-ID range and masked
counts are bitmap arithmetic. The differentiator is the access control, not the scatterplot.

## Where to look

| | |
|---|---|
| [docs/design/](docs/design/) | **The specification.** Start at its [README](docs/design/README.md) — reading order, precedence, and which claims are specified but not yet built |
| [docs/decisions/](docs/decisions/) | Settled decisions, one per file, immutable. Read before re-litigating |
| [docs/agents/](docs/agents/) | **How work is done here** — routing, the design process, the epic lifecycle, parallel work, and the house style |
| [docs/evidence/](docs/evidence/) | Measurements, investigations, prior art. Never normative |
| [probes/](probes/) | Raw measurement campaigns. Re-run before trusting a quoted figure |
| [docs/archive/](docs/archive/) | Frozen. Never cite as authority; never execute |
| GitHub issues | The only authority on what is being worked on |

**A document's standing is its `Status:` line, not its location.** `docs/design/` holds both
normative and provisional documents; the provisional ones say so and name what remains.

**Precedence:** `architecture.md` is the specification and wins. `system-architecture.md` and the
mechanism documents defer to it. Where `contracts.md` and `system-architecture.md` differ, the
eleven deviations in contracts §0.3 govern. `§n` unprefixed means the architecture design.

Every corpus document carries a review trail in its Appendix R.

## Non-negotiables

§4's thirteen invariants are the spec — read them, don't work from memory. The ones most often
broken by a plausible-looking change:

- **I2** — every aggregate must be computable from inside `M_auth` alone. A quantity derived from
  the full dataset and then *gated* is a disclosure, not a filtered view. Accepted exceptions are
  enumerated in Appendix C (C1–C19); anything not in that table is a bug.
- **I7** — sampling happens after masking. Direct evaluation is the **only** selection route: the
  candidate-list alternative was declined ([decision 0008](docs/decisions/0008-candidate-list-route-declined.md))
  and `check-layers.sh` fails if its marker is removed. Deleting the direct path "to simplify"
  blanks the sparsest principals' maps silently.
- **I3 / I12** — labels gate on `M_auth`, never on the filtered mask; filters may move the
  frontier up, never down.
- **I10** — entity IDs never cross the trust boundary. Clients receive an opaque `tessera_id` and
  never evaluate a visibility rule. That identifier is a **blinding permutation, not encryption**
  ([decision 0014](docs/decisions/0014-i10-weakened-to-construction.md)) — do not describe it as a
  cryptographic guarantee, and do not treat it as a defence against a bundle-holder.
- **Deny handling is fail-closed with three distinct retirement rules** (lifecycle §3): deletion
  denies retire by the epoch ledger; suppressions retire *only* on unsuppress (they never touch
  postings); predicate-change entries retire at their compaction fold. Conflating them is
  fail-open — caught in review twice; do not rediscover it. **Two of the three are specified but
  not built**, and are safe today only because nothing retires at all.
- **Pins fix geometry, never authorisation** (lifecycle §2.3). A suppression applies to a pinned
  request the moment it is accepted.

The conformance suite is the deliverable: an implementation that keeps the Morton and Roaring
machinery while quietly dropping I2, I7 or I13b passes every functional test while leaking. Three
of the thirteen invariants are currently covered as designed.

## Working method

**Rust is the implementation language** — engine, build pipeline and serving alike; one binary.
Python is a first-class *consumer* (SDK, supervisor, the test-only reference oracle) and never a
component: no Python in any request path, in artifact production, or in the trusted computing
base. TypeScript is the frontend.

**Design for audit before performance.** Prefer the construction that is obviously correct; keep
modules readable in isolation; keep the query surface narrow — the leak register is exhaustive
*because* the surface is enumerable. New capability enters through the filter contract (§8.2). An
optimisation that costs reviewability needs an argument, not just a benchmark. The measured cost
model to design against: **bitmap operations cost O(containers touched), not O(cardinality)** —
contiguity in entity space is the highest-leverage property in the index.

**Dispatch plans for independent review before implementing.** Hand the plan to a subagent with no
stake in it being right, and act on the review before code is written. This caught four fail-open
paths and two unimplementable mechanisms during design alone.

**Decompose across subagents; direct and review rather than write.** Verify a subagent's work
rather than accepting its summary — invariant-bearing decisions stay with the reviewer.

**Stop and report** rather than guessing, when the answer would set an invariant, a guarantee, or
something the owner has not decided. Full procedures in [docs/agents/](docs/agents/).

## House style

Full guide in [docs/agents/writing.md](docs/agents/writing.md). The rules that matter most:

- **Describe the system, not its construction.** What it is and why — not which revision changed
  it or which phase built it. That archaeology belongs in `docs/decisions/` and git.
- **Module docs carry the design argument**, and run long here where that is warranted: an
  invariant upheld in a way the code does not show, an obvious construction rejected for a
  non-obvious reason, a measurement driving a shape that otherwise looks arbitrary, or a
  deliberate duplication a reader would otherwise "fix". Restating the code is never warranted.
- **Comments record decisions and evidence, not backlog.** There are essentially no `TODO` or
  `FIXME` markers here. Open work is an issue.
- **State negative results.** "F3: NOT confirmed by measurement — do not claim it is" is the form.
  Distinguish measured from modelled from assumed, every time.
- **Mark specified-but-unbuilt machinery at the claim**, with what happens instead. Present tense
  about absent machinery reads as an assurance
  ([decision 0013](docs/decisions/0013-mark-specified-vs-implemented.md)).
- **Prefer stable citations** — `§4`, `contracts §2.5` — over `file.rs:184`, which drifts.
  `scripts/check-doc-links.py` warns on the ones that have visibly rotted.
- **State load-bearing assumptions at the site**, and prefer a test to a comment.
- **Keep emphasis proportionate.** If everything is critical, the reader cannot tell which things
  are — and a small number here genuinely are.
- British spelling, and the established security vocabulary — *conservative label join*, *boolean
  expression indexing*, *partial evaluation*, *Non-Truman model*, *compartmented MAC* — over
  invented terms.

## The gate

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
python3 scripts/check-doc-links.py
```

Run them and read the output before claiming anything passes.
