# Changelog

All notable changes to Mosaica are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). One version covers the `mosaica` binary, the Rust
crates, the `mosaica` and `mosaica-native` Python packages, the `@mosaicajs` npm packages and the
container image.

## [0.1.0] - unreleased

The first public release, an alpha. Formats, the HTTP API and the client libraries may change in
any later release, and a bundle built by one version may be refused by the next.

### Added

- **Interactive maps over very large corpora from one machine.** Mosaica builds a bundle from
  Parquet files, or from data frames in Python, and serves a pannable, zoomable map over it.
  Geometry is stored in Morton order, so a map tile at any zoom is one contiguous range of rows,
  and the server answers a viewport with a sample of points, counts and densities without scanning
  the tile. The build streams within a memory budget, and a build and serve of 10⁹ points on one
  machine has been measured.
- **Any records with a 2D layout**: geographic coordinates, or an embedding projection such as
  t-SNE or UMAP. One corpus can carry several layouts that share item identity, so a viewer can
  switch between them and keep their selection.
- **Filters and search** over categories, numbers, dates, keywords and full text, and over drawn
  regions (box, circle, ellipse and polygon), composable with one another. Highlight mode lights
  the matches and dims the rest.
- **Annotation layers**: clusters, hierarchies including DAGs, regions and hulls, with labels.
- **Item cards** for a selected point, and **bulk reads** of the items and annotations a viewer may
  see, page by page as Apache Arrow tables.
- **Live ingest into a running service.** Insert, delete and suppress items while the server is
  serving; a deletion or suppression applies to every request that starts after it is accepted. A
  write-ahead log makes every accepted write survive a restart, and anything that can be declared
  at a build can be declared at a running service.
- **Per-viewer access control over the whole map.** Each item carries an access label, an
  expression over terms. A viewer's visible set is computed once per session, and every count,
  density, cluster, label and sample the viewer receives is computed from that set alone, not only
  which items they can open. Viewers authenticate with a password, an API key or an OIDC access
  token, or an integrator's backend authorises a session for them.
- **Four ways in:**
  - the HTTP API, described in OpenAPI, which reaches every capability: declare, insert, delete
    and suppress, query, filter and annotate;
  - the Python package `mosaica`, which declares, inserts from data frames, removes, suppresses,
    publishes annotation layers, and queries, filters and counts with pyarrow results, with a
    notebook map widget (`mosaica[widget]`) and the `mosaica-native` platform wheel that carries the
    binary;
  - the TypeScript packages: `@mosaicajs/client`, whose headless store queries and filters the map
    and whose control-plane client declares, ingests, deletes and suppresses, and publishes
    annotation layers; `@mosaicajs/deck` (a deck.gl layer); `@mosaicajs/components` (web
    components and a single-file bundle); and `@mosaicajs/react` (hooks and React wrappers);
  - the `mosaica` command line, which builds a bundle from a declaration and its source files,
    checks, verifies and serves it, reads items, annotations and aggregates in bulk, and manages
    principals, groups, API keys, grants, sessions and OIDC providers. It has no command to insert,
    delete, suppress or annotate on a running server; use the HTTP API or a client for those.
- **Distribution**: release binaries for Linux (glibc and musl, x86_64 and aarch64), macOS (arm64
  and x86_64) and Windows (x86_64); Python wheels on PyPI; npm packages; a multi-architecture
  container image on GHCR; and the Rust crates on crates.io, which are internal to the binary and
  carry no stable API.

[0.1.0]: https://github.com/jennis0/mosaica/releases/tag/v0.1.0
