# TesseraDB
** Access-controlled visual analytics for very large datasets **

> ** Currently in pre-alpha **

TesseraDB is an open-source analytical engine for interactively exploring large collections of records as maps, embedding spaces, clusters, densities, and other visual representations.

A central feature is per-viewer access control. TesseraDB computes the data and derived visual information in a view from the records the requesting user is permitted to see. This applies not only to individual points, but also to counts, densities, samples, clusters and labels.

Filters can combine numeric, date, categorical, spatial and full-text search.

## Why TesseraDB?

Traditional access control is usually applied when a user retrieves a record:

```text
Can this user read record X?
        ↓
      yes/no
```

That isn't always enough for interactive visual analytics.

Suppose a dataset contains sensitive records and a precomputed map contains clusters, counts, density, labels, or other summaries. Even if the underlying records are protected, the shared analytical representation can reveal information about records that a user is not permitted to see.

TesseraDB takes a different approach the permission-filtered set is the input to the analytical operations themselves.

This means that the same dataset can produce different maps and analytical results for different users.

## Built for large, interactive corpora

TesseraDB is designed for datasets that are too large to treat as a browser-side collection of points.

It can work with points in geographic coordinates or other two-dimensional spaces such as embedding projections, while attaching arbitrary structured and textual data to each point.

Filtering can combine:

* numeric fields
* dates
* keywords and categorical fields
* full-text queries
* BM25-style text relevance thresholds
* spatial regions
* coordinate-space conditions
* access-control predicates

These filters can be combined arbitrarily to define the set of records participating in a view.

Note that TesseraDB is deliberately **filter-oriented rather than a ranked search engine**. A text relevance score can be used as a predicate — for example, `BM25 > 8` — but TesseraDB does not currently attempt to turn a query into a ranked list of results. Its purpose is to define the subset of a corpus that should participate in an analytical view.

## The scale

TesseraDB is designed to run on a single machine. It has been tested with:

- 3.5 billion points, with approximately 2 million spatial artifacts, using around 40 GB RAM, built in under 3 hours
- 2.4 million arXiv abstracts with embeddings, including a per-point full-text index, built in 52 seconds

These numbers are intended to demonstrate the scale of the analytical index, not the number of points rendered by a browser. TesseraDB serves viewport-sized and otherwise derived results to a client; the client does not receive billions of points at once.

## Access control extends to derived visual information

TesseraDB treats analytical artifacts as part of the access-controlled view.

For example, a cluster can have rules specifying:

* who may access the cluster;
* a minimum number of visible points;
* a minimum percentage of the cluster's points that the viewer must be able to see;
* whether all contributing points must be visible.

The representation of an artifact can also be derived from the points visible to the current viewer. For example, a cluster boundary or centroid can change when some of its contributing points are hidden.

Different access policies can also expose different labels for the same spatial artifact.

This is not the same as running a completely new clustering algorithm for every user. TesseraDB currently derives viewer-specific representations from declared analytical artifacts; it does not discover entirely new clusters separately for every viewer.

## A changing dataset

TesseraDB is not limited to static datasets.

New records can be continuously ingested into a running service, and records can be deleted or suppressed. Once a deletion or suppression has been accepted, subsequent requests do not expose the affected records.

This makes TesseraDB suitable for analytical views over corpora that continue to change rather than only for static benchmark datasets.

## What can TesseraDB represent?

A TesseraDB corpus can contain, for each point:

* one or more coordinate systems
* numeric values
* dates
* categorical or keyword fields
* arbitrary text
* full-text indexes
* access-control labels
* metadata used by analytical layers

Coordinates do not have to represent geography. A corpus can, for example, be projected into an embedding space and explored spatially.

This makes the same underlying engine applicable to things such as:

* geographic datasets
* document and research corpora
* embedding spaces
* entity collections
* large collections of records with heterogeneous access policies

## TesseraDB is a serving engine, not the visualisation

TesseraDB provides the analytical data and derived representations.

The visual interface is a separate client. TesseraDB currently provides integrations including a web component, deck.gl integration, React bindings, a Python notebook widget, and an HTTP API.

This separation means TesseraDB can sit underneath an application with its own user interface rather than requiring a particular visualisation framework.

---

### In one sentence

**TesseraDB is an analytical engine for exploring billion-scale spatial or embedding datasets interactively, where both the underlying records and the derived visual analytics obey each viewer's access permissions.**

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

[Apache-2.0](LICENSE). Copyright 2026 Joe Ennis.
