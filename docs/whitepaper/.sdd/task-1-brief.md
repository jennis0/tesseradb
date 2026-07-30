### Task 1: Fact extraction

Produces the cited fact sheets every later prose task depends on. Dispatch all six subagents in a single message so they run concurrently.

**Files:**
- Create: `docs/whitepaper/facts/part-1.md` … `part-6.md`

**Interfaces:**
- Produces: six markdown fact sheets. Each entry is one line: `- CLAIM — source: <file> §<section>` with a verbatim supporting quote where the claim is a number or a third-party assertion.

- [ ] **Step 1: Create the facts directory**

```bash
mkdir -p /home/joe/code/tessera/docs/whitepaper/facts
```

- [ ] **Step 2: Dispatch six fact-extraction subagents concurrently**

Each subagent gets this prompt shape, with `<PART>`, `<DOCS>` and `<TOPICS>` filled in from the table below:

> Read `<DOCS>` (pass paths explicitly — `.ignore/` is skipped by default search tooling). Extract every fact needed to write about `<TOPICS>`. Return a markdown fact sheet. Each entry is one line: `- CLAIM — source: <file> §<section>`, plus a verbatim quote for any number or any assertion about a third-party system. Preserve the distinction between demonstrated and marketed capability wherever the source draws it. Do not write prose, do not summarise into narrative, do not include anything you cannot cite. If a claim you expect to find is absent from the sources, say so explicitly rather than supplying it.

| Part | DOCS | TOPICS |
|---|---|---|
| 1 | `.ignore/prior-art-synthesis.md`, `.ignore/prior-art-2-visual-analytics.md`, `.ignore/prior-art-3-databases.md`, `.ignore/prior-art-4-authorization-disclosure.md` | Aggregate leakage in systems with per-document security; demonstrated vs marketed scale for every surveyed visualisation system; row-level access control coverage across the category; PostgreSQL RLS and DuckDB measurements |
| 2 | `.ignore/tessera-architecture-design.md` §5, §10.3, §10.4, `probes/results.md` §4, §5, §6, `probes/optimisations.md` | Entity space vs row space and the permutation; Morton ranking and tile-to-range contiguity; Roaring container structure; the O(containers touched) cost model |
| 3 | `.ignore/tessera-architecture-design.md` §6, §2.6, §4, `.ignore/tessera-contracts-spec.md` (plugin ABI), `probes/results.md` §3, §5, `probes/optimisations.md` | The authorisation plugin boundary; DNF terms, the term index, postings and union; mask construction; signature-sorted allocation and I9 permanence; the end-to-end request path |
| 4 | `.ignore/tessera-architecture-design.md` §7, §8, §12, §4 | Level of detail, priority nesting, direct evaluation, I7; label gating and the containment frontier, I3/I12; the two-mask model; compartmented partitions and required-set gating |
| 5 | `.ignore/tessera-concurrency-lifecycle.md`, `.ignore/tessera-architecture-design.md` §11 | WAL and the ack contract; buffer, watermark and overlay; segments, merging, compaction; generations and pins; the three retirement rules and why conflating them is fail-open |
| 6 | `probes/results.md`, `probes/phase0-memo.md`, `.ignore/tessera-architecture-design.md` §13, §16, Appendix C | The 10⁹ measurements with their conditions; the 8.8 s permutation cost; what Phase 0 did and did not establish; open questions; the leak register C1–C16 |

- [ ] **Step 3: Verify each fact sheet is citable**

For each of the six files, confirm every line carries a `source:` reference. Spot-check three numeric claims per sheet against the named section by reading it directly. A fact sheet with uncited lines is rejected — send it back to the subagent rather than repairing it yourself.

- [ ] **Step 4: Checkpoint**

Confirm six files exist and are non-empty. Nothing to build yet.
