# Using Tessera

*Skeleton.* Pages in this set and what each will hold.

- **what-tessera-does.md**: the feature list. A masked map over a corpus; views and view groups; filters over categories, numbers, dates, keywords and text; typeahead; drawn regions; annotation layers with clusters, hierarchies, regions and hulls; highlight; item cards; ingest into a running service; suppression and deletion.
- **quickstart.md**: a thousand-row corpus, `corpus.toml`, `tessera.toml`, the identity key and credentials, `tessera check`, `tessera build`, `tessera serve`, a browser. The GeoNames recipe in `test_corpora/geonames/README.md` is the working example today.
- **configuration.md**: every key of `corpus.toml` and `tessera.toml`, its type, default and what refuses. Extracted from `design/configuration.md` §1, §5, §7 and §8 and from `crates/tessera-server/src/config.rs`.
- **operating.md**: the three planes and their credentials, ingest and denies, compaction and its schedule, health and readiness, what the reports mean.
- **clients/**: the TypeScript packages (`@tesseradb/client`, `@tesseradb/components`, `@tesseradb/deck`, `@tesseradb/react`), per-component attributes, events, parts and slots; theming through the `--tessera-*` tokens; use from React and from plain custom elements; the Python `tesseradb` widget; writing a client against the wire alone (the twelve rules the server cannot enforce).
