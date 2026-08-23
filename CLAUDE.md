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
| [docs/roadmap.md](docs/roadmap.md) | What constrains the **order** of work — themes, dependencies, and the couplings that force serialisation. Never a status record |
| [docs/evidence/](docs/evidence/) | Measurements, investigations, prior art. Never normative |
| [probes/](probes/) | Raw measurement campaigns. Re-run before trusting a quoted figure |
| [docs/archive/](docs/archive/) | Frozen. Never cite as authority; never execute |
| GitHub issues | The authority on what is being worked on, everywhere except the artifact work |
| [docs/artifact-delivery.md](docs/artifact-delivery.md) | **The artifact (annotations) work** — its stages, its gates and its status, tracked here rather than on issues by owner direction |

**A document's standing is its `Status:` line, not its location.** `docs/design/` holds both
normative and provisional documents; the provisional ones say so and name what remains.

**Precedence:** `architecture.md` is the specification and wins. `system-architecture.md` and the
mechanism documents defer to it. Where `contracts.md` and `system-architecture.md` differ, the
eleven deviations in contracts §0.3 govern. `§n` unprefixed means the architecture design.

**`write-path.md` is normative for the write path** (promoted 2026-08-04) — ingest, the commit
window, the WAL's write half, flush, the deny lane and the overlay, merge, and compaction's seam.
It absorbed `flush-and-merge.md`, which is deleted, and the write-side sections of
`concurrency-lifecycle.md` and `system-architecture.md`, which point at it; its §13 is the map of
what moved. Lifecycle keeps the read path, retention, WAL *recovery* and caching.

Every corpus document carries a review trail in its Appendix R.

## Non-negotiables

§4's thirteen invariants are the spec — read them, don't work from memory. The ones most often
broken by a plausible-looking change:

- **I2** — every aggregate must be computable from inside `M_auth` alone. A quantity derived from
  the full dataset and then *gated* is a disclosure, not a filtered view. Accepted exceptions are
  enumerated in Appendix C (C1–C26); anything not in that table is a bug.
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
- **Deny handling is fail-closed, with two removal rules that must never be conflated**
  (write-path §5.4; ruled 2026-08-03, superseding the stamp ledger): suppressions retire *only*
  on unsuppress (they never touch postings — Rule S); deletions — and any legacy
  predicate-change entries, the op being withdrawn (decision 0047: edit is delete + re-ingest,
  and a deleted holder never blocks the re-ingest) — retire *only* at the compaction fold that
  executes them (Rule F). Giving a suppression any other retirement route is fail-open — caught
  in review twice; do not rediscover it. The fold exists, retires, and is scheduled — a nightly
  gated window and four any-hour gauges (compaction §9 — now **normative** — and decisions 0056,
  0057).
- **Geometry stamps are advisory, never authorisation** (decision 0041 — pins are deleted). A
  suppression applies to every request the moment it is accepted, whatever stamp was presented.

The conformance suite is the deliverable: an implementation that keeps the Morton and Roaring
machinery while quietly dropping I2, I7 or I13b passes every functional test while leaking. Seven
rows of the matrix are covered; **five have no coverage — I5, I6, I8, I11 and I13b — and only one
of them, I6, is uncovered for want of an implementation to test**. There is no wasmtime host, so
nothing can be asked of a guest plugin. The rest are a testing gap: the annotation machinery I8
needs is built and enforced, I12's frontier half is built in the form that replaced the frontier
(every artifact tested on its own — decisions 0080, 0082, 0083), and I11's cover was deleted with
the pin that carried it. I5 and I13b sit between the two, each needing a second partition or a
genuinely divergent plugin before an oracle could disagree at all.

*We cannot test this* and *we have not tested this* are different claims, and only the first is an
excuse — a register that records built machinery as absent understates its own gap. Corrected
2026-08-19; **I3 then moved to covered 2026-08-20**, the first row to move because a test was
written rather than because machinery arrived. `conformance.md` r15 carries the per-row reasons, and
its §0 carries the other half of that day: the suite had not run at all since the configuration
rework changed a CLI flag under it, and the 27 failures that surfaced when it could were the mask
catalogue's own assumptions about term ids and entity ids. Both are fixed; it is **432 of 432**.

## What the strictness is for

**This is a secure database, not an assured system.** The fail-closed posture above is real and it
is narrow. Three questions decide how strict to be, and only the first two earn a refusal.

- **Does it leak?** Anything a principal can observe — masks, gates, membership requirements, a
  disclosure control accepted and never enforced. Fail closed, no defaults, refuse the
  unenforceable. This boundary is small and enumerable, which is the point: §4's invariants and
  Appendix C are its whole extent.
- **Is it irreversible?** Entity ids are permanent, term ids determine them, a published identity is
  tombstoned. A rerun does not undo these, so they get the strict treatment though they leak
  nothing.
- **Everything else is recoverable and discloses nothing** — frames, joins, coverage, roster shape,
  config ergonomics, defaults. **Report loudly, let the operator decide, and do not block a build.**
  The operator is present and the loop is fast: a wrong extent costs a rerun, not a mission.

Refusing outside the first two cases is not the safe option, it is *an* option — one that moves the
cost onto the caller while looking principled in a way that "this is now harder to use" does not.
Before adding a refusal outside the disclosure surface, say why it is worth blocking a build; if
the answer is only *it might be wrong*, make it a warning that prints the numbers, and choose a
denominator that means something (entities covered, not rows dropped). Prefer **ignore-and-report**
over **refuse** for joins and inputs.

This is the same rule as *keep emphasis proportionate*, applied to behaviour rather than prose: if
everything is a refusal, the refusals protecting the invariants stop standing out.

## Working method

**Rust is the implementation language** — engine, build pipeline and serving alike; one binary.
Python is a first-class *consumer* (SDK, supervisor, the test-only reference oracle) and never a
component: no Python in any request path, in artifact production, or in the trusted computing
base. TypeScript is the frontend.

**Pre-release: accept *zero* cost for backwards compatibility.** This is the only Tessera that
exists — no deployment, no bundle, no WAL and no client outside this repository
([decision 0048](docs/decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)). So
"a reader might still hold one" is never a live premise, and a format may be changed freely
provided the artifacts are recreated: reorder an enum, rename a field, drop a column. Do not
append a variant to preserve a discriminant, do not `#[serde(default)]` a field so an older
bundle still opens, and do not carry a shape whose only justification is a state some earlier
version could have produced. Version numbers still move when discriminants shift — a bump makes a
stale local artifact a loud refusal instead of a silent misread — but that is a fail-closed guard,
not compatibility.

The line this does *not* cross is [0048](docs/decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)'s
own: compatibility with a **past** does not exist, defences for the **present** do. Fail-closed
guards, the contracts a second reader depends on (the Python oracle, the conformance suite), and
the format-stability rules of a *running* process — `seg_id` never reused, dictionary extents
positional — all stay.

**Build is ingest into an empty database** ([decision 0091](docs/decisions/0091-build-is-ingest-into-an-empty-database.md)).
There is no difference in functionality or user experience between the two entry points; what
differs is cost and acquisition. A feature that works at a build and not at ingest is unfinished
rather than staged, and a build-only refusal is a bug unless it is about where rows come from. The
rule is about functionality and client experience; internals may differ freely, and two do —
entity ids are assigned in signature-sorted order at a build and above the high-water at ingest,
and a build packs in one pass because nothing is being served.

**Design for audit before performance.** Prefer the construction that is obviously correct; keep
modules readable in isolation; keep the query surface narrow — the leak register is exhaustive
*because* the surface is enumerable. New capability enters through the filter contract (§8.2). An
optimisation that costs reviewability needs an argument, not just a benchmark. The measured cost
model to design against: **bitmap operations cost O(containers touched), not O(cardinality)** —
contiguity in entity space is the highest-leverage property in the index.

**Stop and report** rather than guessing, when the answer would set an invariant, a guarantee, or
something the owner has not decided. Full procedures in [docs/agents/](docs/agents/).

## Pace

The process in [docs/agents/](docs/agents/) protects thirteen invariants and a leak register. It
is not a tax on every change, and applying it where it does not belong has a visible cost here:
revision archaeology crowding design out of the corpus, review rounds that add mechanism instead
of removing it, and backlogs of issues for things that should just have been fixed.

- **Fix it now.** If a problem is in scope and you can fix it in the change you are making, do
  that. An issue is for work that is genuinely large, needs an owner ruling, or is a deliberate
  deferral — not for a loose end you noticed. A change that spawns several issues has usually
  mistaken a to-do list for a plan.
- **Review once, where it counts.** One adversarial review at the point a design becomes binding
  — not a round per draft. Findings are dispositioned in a single pass and the document is
  promoted; re-review only if the disposition changed the design's shape. Reviews that keep
  finding things are usually growing the design, not converging it.
- **Delegate for breadth, not for assurance.** A subagent earns its cost on a large,
  genuinely independent track of work — a wide investigation, a parallel implementation seam.
  Do not delegate what you would finish in a handful of tool calls, and do not spawn a subagent
  to double-check work you can check yourself. One agent where one will do.
- **Verify a subagent's work** rather than accepting its summary; invariant-bearing decisions
  stay with the reviewer.
- **Scope is the deliverable.** Deliver what was asked at the scope intended. If a better
  approach or a real problem turns up, say so in a sentence and continue rather than quietly
  widening the task.

## Talking to the owner

Joe knows this system better than you do. When he asks a question he is usually thinking out loud
and wants a peer to think with, not a verdict with the reasoning arranged behind it. Answer what he
asked and leave the decision with him. Agreement is a complete reply.

Four constructions do not belong in a message. Predecessor models produced none of them across
1,200 messages in this repository, so these are things to drop, not habits to moderate.

- **Commentary on your own messages** — "I was wrong", "what I should have said", "to be precise".
  If an earlier answer was wrong, just say the right thing now.
- **Filler that announces importance** — "the real question", "the key thing", "importantly",
  "worth noting". If a sentence's only job is to say the next one matters, cut it.
- **Rules invented from a conversation.** A question about a column gets an answer about that
  column, not a principle that has to be unpicked later.
- **Uniform urgency.** Save *fail-open*, *silent* and *breaks* for the things that are.

State uncertainty once and plainly — "I don't know whether X", or just ask — rather than hedging
spread through every paragraph.

Match structure to content: a list when the content is a list, lettered options when a decision is
needed so he can reply with a letter, plain paragraphs otherwise. Do not impose headed sections on
prose. Length follows the question — a long answer he can skip through beats a short one that costs
him three follow-ups.

Identifiers go in brackets or not at all. He is not holding the corpus in his head when he reads.

## House style

This governs what is written **into the repository** — design documents, module docs, decisions,
commit messages. It does not govern messages, which the section above covers; a summary of it here
is read every session and ends up in chat, which is why the summary is a pointer.

Read [docs/agents/writing.md](docs/agents/writing.md) before writing corpus prose. Four rules are
repeated here because they are violated silently rather than visibly:

- **Mark specified-but-unbuilt machinery at the claim**, with what happens instead. Present tense
  about absent machinery reads as an assurance
  ([decision 0013](docs/decisions/0013-mark-specified-vs-implemented.md)).
- **State negative results.** "F3: NOT confirmed by measurement — do not claim it is" is the form.
  Distinguish measured from modelled from assumed, every time.
- **Comments record decisions and evidence, not backlog.** There are essentially no `TODO` or
  `FIXME` markers here. Open work is an issue.
- **Prefer stable citations** — `§4`, `contracts §2.5` — over `file.rs:184`, which drifts.
  `scripts/check-doc-links.py` warns on the ones that have visibly rotted.

British spelling throughout, and the established security vocabulary — *conservative label join*,
*boolean expression indexing*, *partial evaluation*, *Non-Truman model*, *compartmented MAC* —
over invented terms.

## The gate

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-clients.sh
python3 scripts/check-doc-links.py
```

Run them and read the output before claiming anything passes.

**`--no-fail-fast`, and read the count.** Without it cargo stops at the first failing binary and
skips the rest, so a run that reports no failures alongside a *smaller* passing total reads as
success. That has already been mistaken for a green gate here.

**The TypeScript client is in the gate**, and is there because one rename shipped three defects
into `clients/` — two app-state fields collapsed onto one name, a `.slice()` call renamed to
`.view()`, and a shadowed `const` that threw before its initialiser ran — none caught, each found
later by a separate investigation. A client that does not compile is not a smaller failure than a
crate that does not compile. The operator `.mjs` scripts are typechecked with `checkJs` rather than
parsed, because the third defect is a type error and not a syntax one.
