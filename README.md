# Tessera

**A permission-masked point service.** An interactive, pannable and zoomable map over a large
document corpus, where what a viewer may see determines not just which items they retrieve, but
every count, density, cluster and summary they are shown.

Most systems with per-document security draw their line at *retrieval*: a viewer cannot open a
record they lack access to, but they can still learn it exists from a count, a cluster boundary or
a density gradient. Tessera moves the line. A viewer's visible set is materialised once per session
as a Roaring bitmap, and every count, sample, label decision and density estimate is computed from
that set alone.

The differentiator is the access control, not the scatterplot.

## How it works, in one paragraph

Permissions live in **entity space**; geometry lives in **row space**; an explicit permutation is
the only path between them. Geometry is stored in Morton (Z-order) sequence, so a quadtree tile is
a *contiguous range of row IDs* — which turns an exact masked count into bitmap arithmetic rather
than a scan. A viewport costs work proportional to the rows the viewer can see in it, not to the
size of the corpus.

## Status

**Not ready to deploy.** The engine builds and serves; the guarantees are specified and only
partly enforced.

- The build pipeline, the write-ahead log, entity-ID allocation, the segment loader, mask
  composition and the viewport query all work, measured against a synthetic 10⁹-point corpus.
- **Three of the thirteen invariants are covered by the conformance suite as designed.** The suite
  is the deliverable, and it is incomplete.
- Two of the three deny-retirement rules are specified but unbuilt. They are safe today only
  because nothing retires at all.
- Compaction, merge, partitions, labels and the multi-process split are specified and unbuilt.

Everything specified but not yet built is marked ⊘ at the point it is claimed, and counted in
[`docs/design/inventory.md`](docs/design/inventory.md). Work in progress is tracked as capability
epics in this repository's issues.

## Security posture

Read this before evaluating the system.

**What is enforced structurally.** Aggregates are computable only from inside the authorised mask —
the mask is the sole entry point to the geometry arrays, so there is no path along which an
aggregate over unauthorised rows can be constructed. Entity IDs never reach a client: no
request-path artifact stores one, so the gather cannot produce one. Sampling happens after masking,
never before, and the floor clause that keeps a sparse viewer's map from going blank cannot be
switched off — a zero floor is refused at startup rather than clamped.

**What is accepted rather than eliminated.** Nineteen residual disclosures are enumerated in the
leak register (`docs/design/architecture.md` Appendix C), with severity, mitigation and status for
each. The register is exhaustive by construction: a disclosure not in that table is a bug, not an
omission. That exhaustiveness depends on the query surface staying about five shapes wide, which is
a deliberate constraint rather than an early-stage limitation.

**What is claimed less than you might assume.** The client-facing identifier is a *blinding
permutation*, not encryption — an 8-round Feistel over a non-cryptographic mixer. It prevents a
viewer-plane client from correlating or enumerating entity IDs. It is **not** a cryptographic
guarantee and **not** a defence against anyone holding the bundle, who obtains the key by
construction.

**What is not yet enforced.** See Status above, and the ⊘ markers. In particular, a partition not
consulted failing closed (I13b) has no implementation and no test.

## Scale and cost

Measured on a synthetic 10⁹-point corpus on a single 47 GiB box:

| | |
|---|---|
| Corpus | 10⁹ points, ~130 terms per item |
| Bundle | ~47 GB on disk |
| Viewport latency | 135–164 ms p50 at 10⁹; selection is 83–89% of it |
| Cost driver | Rows visible in the viewport. Uncorrelated with the number of points returned |
| Build | Streaming with external spill; memory-bounded by a pre-flight plan |

These are synthetic-corpus figures. Three of the headline results depend on how a deployment's
access labels are actually distributed, and should be re-measured against real labels before being
relied on. Raw records are in [`probes/`](probes/).

## Repository

| | |
|---|---|
| [`docs/design/`](docs/design/) | **The specification.** Start at its [README](docs/design/README.md) |
| [`docs/decisions/`](docs/decisions/) | Settled decisions, one per file, immutable |
| [`docs/agents/`](docs/agents/) | How work is done here — process, parallelism, house style |
| [`docs/evidence/`](docs/evidence/) | Measurements, investigations, prior art. Never normative |
| [`probes/`](probes/) | Raw measurement campaigns |
| [`crates/`](crates/) | The Rust workspace — one binary, `tessera build` and `tessera serve` |
| [`clients/ts/`](clients/ts/) | The TypeScript client and a deck.gl viewer |
| [`conformance/`](conformance/), [`reference/`](reference/) | The conformance suite and its independent Python oracle |

## Who this is for

- **Evaluating the approach, or assuring it** → [`docs/design/README.md`](docs/design/README.md)
  is the guided tour: the guarantees, how they are enforced, and what is accepted.
- **Contributing, human or agent** → [`docs/agents/README.md`](docs/agents/README.md).
- **Deploying it** → not yet. There is no packaging story and no operations guide, because there is
  nothing worth deploying until the conformance suite is complete.

## Licence

Not yet determined.
