# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

**Phase 1 — implementation.** Phase 0 is complete: measurements over the synthetic 10⁹ corpus are in [probes/](probes/) (`dataset.md`, `results.md`, `optimisations.md`, `phase0-memo.md`), the verdict is go, and their conclusions are folded into the design corpus. There will be **no real-label rerun** — this is a personal project with no real access-labelled corpus available; synthetic-corpus evidence is accepted as final (design r18), and the caveat survives only as deployment guidance: any future deployment with real labels should re-run the Phase 0 measurements before trusting the policy-dependent headlines (signature alignment, posting compression, union cost).

Scaffolding the Cargo workspace and writing Phase 1 code is now the job. Phase 1 scope is the walking skeleton (plan §5): the tiler, the WAL and its ack contract, the entity-ID allocator **with signature-sorted assignment from day one** (permanent under I9 — cannot be retrofitted), the segment loader, term index and mask build, the viewport query, the handle allocator, and the differential oracle scaffolding. Crate layout is system architecture §3; bundle and API byte formats are the contracts spec; runnable analysis models are in [docs/evidence/analysis/](docs/evidence/analysis/) and the probe scripts in [probes/](probes/).

## The documents

All design docs in [docs/design/](docs/design/); supporting evidence — prior art, the scaling analysis and its models — in [docs/evidence/](docs/evidence/). **Precedence:** the architecture design is the specification; the mechanism documents implement it and defer to it; where the contracts spec and the system architecture's sketches differ, the contracts spec's §0.3 deviations govern. Every document carries its review trail in an Appendix R — read it before re-litigating a decision.

| | |
|---|---|
| [README.md](docs/design/README.md) | Start here. Reading order, what is settled, what was measured |
| [architecture.md](docs/design/architecture.md) | **The specification** (r24). §2.6 walks a request end to end; §4 is the thirteen invariants; Appendix C is the leak register; Appendix G the revision history |
| [system-architecture.md](docs/design/system-architecture.md) | The built system (r5): processes and planes, crate decomposition, lifecycle, config, packaging; decisions D1–D16 |
| [contracts.md](docs/design/contracts.md) | Byte level (r11): bundle format, service API, plugin ABI, wire |
| [concurrency-lifecycle.md](docs/design/concurrency-lifecycle.md) | Mechanisms (r4): generations, pins, the **three** retirement rules, WAL, merge-vs-snapshot, router/worker protocol |
| [conformance.md](docs/design/conformance.md) | The suite (r3): definitions-oracle, canonicalised canaries, byte-scanner, eight interleaving scripts |
| [implementation-plan.md](docs/design/implementation-plan.md) | Phases, conformance matrix (§10), effort sizing |
| [visualisation.md](docs/design/visualisation.md) | The client: two profiles, one data contract (deferred; backend first) |
| [docs/evidence/](docs/evidence/) | Non-normative evidence: [scaling-analysis.md](docs/evidence/analysis/scaling-analysis.md) with its runnable models, and the five [prior-art](docs/evidence/prior-art/) reviews behind the build-vs-buy verdict |
| [probes/](probes/) | Phase 0 corpus, measurements, engineering distillation — real numbers; re-run before trusting any figure quoted from them |

`§n` in any document refers to the architecture design unless prefixed (SA §n, contracts §n). Read the relevant section before changing anything it governs; these documents argue their decisions, and most obvious objections are already answered in them.

## What this is

A permission-masked point service: a pannable, zoomable map over a document corpus where what a viewer may see determines not just which items they retrieve but **every count, density, cluster and summary they are shown**. A viewer's visible set is materialised once per session as a Roaring bitmap; geometry is stored in Morton order so a tile is a contiguous row-ID range and masked counts are bitmap arithmetic. The differentiator is the access control, not the scatterplot.

## Non-negotiables

§4's thirteen invariants are the spec — read them, don't work from memory. The ones most often broken by a plausible-looking change:

- **I2** — every aggregate must be computable from inside `M_auth` alone. A quantity derived from the full dataset and then *gated* is a disclosure, not a filtered view. Accepted exceptions are enumerated in Appendix C (C1–C16); anything not in that table is a bug.
- **I7** — sampling happens after masking. Direct evaluation is the **main** selection route (measured), not a fallback; deleting it "to simplify" blanks the sparsest principals' maps silently.
- **I3 / I12** — labels gate on `M_auth`, never on the filtered mask; filters may move the frontier up, never down.
- **I10** — entity IDs never cross the trust boundary. Clients get per-session opaque handles and never evaluate a visibility rule.
- **Deny handling is fail-closed with three distinct retirement rules** (lifecycle §3): deletion denies retire by the epoch ledger; suppressions retire *only* on unsuppress (they never touch postings); predicate-change entries retire at their compaction fold. Conflating them is fail-open — this was caught in review twice; do not rediscover it.
- **Pins fix geometry, never authorisation** (lifecycle §2.3). A suppression applies to a pinned request the moment it is accepted.

The conformance suite is the deliverable (plan §10.1, conformance design): an implementation that keeps the Morton and Roaring machinery while quietly dropping I2, I7 or I13 passes every functional test while leaking.

## Working method

**Rust is the implementation language** — engine, build pipeline (`tessera build`) and serving alike; one binary. Python is a first-class *consumer* (SDK, supervisor, the test-only reference oracle) and never a component: no Python in any request path, in artifact production, or in the trusted computing base. TypeScript/JavaScript is the (deferred) frontend.

**Design for audit before performance.** Prefer the construction that is obviously correct; keep modules readable in isolation; keep the query surface narrow (five viewer verbs — the leak register is exhaustive *because* the surface is enumerable). New capability enters through the filter contract (§8.2). An optimisation that costs reviewability needs an argument, not just a benchmark. The measured cost model to design against: **bitmap operations cost O(containers touched), not O(cardinality)** — contiguity in entity space is the highest-leverage property in the index.

**Dispatch plans for independent review before implementing.** For anything non-trivial, write the plan first and hand it to a subagent to review against the design documents and the invariants, with no stake in the plan being right. Act on that review before code is written. This method caught four fail-open paths and two unimplementable mechanisms during the design phase alone — it works; keep it.

**Decompose implementation across subagents; direct and review rather than write.** Verify a subagent's work rather than accepting its summary — invariant-bearing decisions stay with the reviewer, not the worker.

## Conventions

British spelling (*authorisation*, *visualisation*, *licence*) and the established security vocabulary from the synthesis's terminology table — *conservative label join*, *boolean expression indexing*, *partial evaluation*, *Non-Truman model*, *compartmented MAC* — in preference to invented terms. Each brings a literature with it, and a security reviewer will find the lineage anyway.
