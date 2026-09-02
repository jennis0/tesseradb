# CLAUDE.md

## What this is

Tessera is a permission-masked point service: a pannable, zoomable map over a document corpus, where what a viewer may see determines every count, density, cluster and summary they are shown, not only which items they can retrieve. A viewer's visible set is computed once per session as a Roaring bitmap. Geometry is stored in Morton order, so a tile is a contiguous row-id range and a masked count is bitmap arithmetic.

## Where to look

| | |
|---|---|
| [docs/design/](docs/design/) | The specification. Start at its [README](docs/design/README.md). `architecture.md` wins any conflict; §n with no prefix means that document |
| [docs/decisions/](docs/decisions/) | Settled decisions, one per file. Read before reopening one |
| [docs/agents/](docs/agents/) | How work is done here, and [writing.md](docs/agents/writing.md), the house style |
| [docs/roadmap.md](docs/roadmap.md) | What constrains the order of work. Not a status record |
| [docs/evidence/](docs/evidence/), [probes/](probes/) | Measurements and investigations. Not normative; re-run a figure before relying on it. Superseded material is in git history, not in the tree |
| GitHub issues | What is being worked on. [docs/ingest-campaign.md](docs/ingest-campaign.md) tracks the test-corpus ladder |

A document's `Status:` line says whether it is normative or provisional. The directory does not.

## Invariants

The thirteen invariants are in architecture §4. Read them there. The ones most often broken by a plausible change:

- **I2.** Every aggregate is computed from inside the viewer's mask. Computing over the full dataset and then gating is a disclosure. Appendix C lists the accepted exceptions; anything not in that table is a bug.
- **I7.** Sampling happens after masking. Direct evaluation is the only selection route ([decision 0008](docs/decisions/0008-candidate-list-route-declined.md)); `check-layers.sh` fails if its marker is removed. Do not remove it to simplify: sparse principals' maps go blank with no error.
- **I3 / I12.** Labels gate on the authorised mask, never on the filtered one. Filters can hide, never reveal.
- **I10.** Entity ids never reach a client. The `tessera_id` is a blinding permutation, not encryption ([decision 0014](docs/decisions/0014-i10-weakened-to-construction.md)); do not describe it as a cryptographic guarantee or as a defence against a bundle-holder.
- **Two deny removal rules** (write-path §5.4). A suppression leaves the overlay only on unsuppress. A deletion leaves it only at the compaction that removes its rows. Any other removal route re-exposes items.
- **Geometry stamps are advisory** (decision 0041). A suppression applies to every request from the moment it is accepted.

Coverage of the invariants is stated in `conformance.md` §4.6 and nowhere else.

## How strict to be

Refuse only where a change leaks (anything a principal can observe) or is irreversible (entity ids, term ids, a published identity). Everything else is recoverable: report it, print the numbers with a meaningful denominator, and let the operator decide. Do not block a build because a result might be wrong. For joins and inputs, ignore and report rather than refuse.

## Working method

- **Rust throughout**, one binary. Python is a consumer (SDK, supervisor, the test-only oracle), never in a request path, artifact production, or the trusted computing base. TypeScript is the frontend.
- **No backwards compatibility before release** ([decision 0048](docs/decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)). Change formats freely and recreate the artifacts. No `#[serde(default)]` for old bundles, no appended variants to keep a discriminant. Bump the version when a discriminant changes so a stale artifact is refused. Keep the contracts a second reader depends on (the Python oracle, the conformance suite) and the rules of a running process (`seg_id` never reused, dictionary extents positional).
- **Build is ingest into an empty database** ([decision 0091](docs/decisions/0091-build-is-ingest-into-an-empty-database.md)). A feature that works at build and not at ingest is unfinished. Internals may differ.
- **Audit before performance.** Prefer the construction that is obviously correct; keep the query surface narrow (the leak register is exhaustive because the surface is enumerable); add capability through the filter contract (§8.2). Cost model: bitmap operations cost by containers touched, not by cardinality.
- **Stop and report** when an answer would set an invariant, a guarantee, or something Joe has not decided.

## Pace

- Fix it now. An issue is for large work, a deferral, or an owner ruling, not for a loose end.
- Review once, when a design becomes binding. Re-review only if the fixes changed its shape.
- Delegate for breadth, not assurance. One agent where one will do. Verify a subagent's work; do not accept its summary.
- Deliver the scope asked for. Mention a better approach in a sentence and continue.

## Talking to Joe

He knows the system better than you do and is usually thinking out loud. Answer what he asked and leave the decision with him. Agreement is a complete reply. Do not comment on your own earlier messages; say the right thing now. No filler that announces importance. No rules generalised from one conversation. Save "fail-open", "silent" and "breaks" for cases that are. State uncertainty once. Use lettered options when he needs to decide. Identifiers go in brackets or not at all.

## House style

Read [docs/agents/writing.md](docs/agents/writing.md) before writing prose into the repository. Mark anything specified but not built at the point you claim it. Say whether a figure is measured, modelled or assumed. No `TODO` or `FIXME`; open work is an issue. British spelling.

## Checks before finishing

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-test-reachability.sh --quick
bash scripts/check-clients.sh
bash clients/py/check.sh
python3 scripts/check-doc-links.py
```

Read the output before claiming a pass. Keep `--no-fail-fast` and check the test count: without it cargo stops at the first failing binary and a smaller passing total looks green. The TypeScript check is in the gate because the Rust build cannot see a broken client.
