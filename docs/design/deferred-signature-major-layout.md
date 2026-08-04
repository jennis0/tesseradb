# Row-space signature-major layout

**Status:** Deferred — a sketch, **explicitly not approved**. Recorded so the option is not lost and so its open problems are not rediscovered from scratch. Nobody should treat this as a design; it is the material a design would start from.

**What it would take to become one:** three inputs, none of which exist yet: a working conformance suite, the gather probe, and a real-label signature histogram showing the knee. The first is invariant-bearing — the whole-group visibility shortcut is sound only while a group is untouched by the overlay and the live set, which is exactly the class of change that passes every functional test while leaking.

---

**Row-space signature-major layout (probes, optimisations §4) — needs deciding, not before Phase 2.** Sorting rows by (signature, morton) for the largest signature groups with a Morton-only residual. It is a *per-deployment build decision*, but it is listed here because two of its consequences reach the core and one of them is a format change.

*What is settled.* The key is the signature, never a single term — items carry ~130 terms each, so term-major would duplicate geometry rows and cost I2 the property that a masked count is a bitmap cardinality. Priority cannot be promoted above the Morton prefix to recover contiguity: that buys one tile depth at the cost of every depth below it. Build-time hierarchical LOD levels are closed by invariant, not cost — the top-*k* would be computed unmasked, which is the I2 shape §7.2 opens by rejecting.

*What decides it.* Three inputs, none of which exist yet:
1. **A working conformance suite** (Phase 2, §10.1). The whole-group visibility shortcut is sound only while a group is untouched by the overlay and the live set — invariant-bearing, and exactly the class of change that passes every functional test while leaking.
2. **The gather probe** (probes, optimisations §3.5). Phase 0 measured no column read at all, so the retrieval half of the case is modelled. The read that matters is the *priority* column under direct evaluation, which touches every visible row in a tile range rather than *k*.
3. **A real-label signature histogram showing the knee.** Unavailable to this project (design r18) and therefore deployment guidance; on the synthetic corpus the top 500 groups cover 82.4%, while author-like policy (1.54M signatures over 2.42M items) degrades it to nothing.

*What it touches if adopted.* Per-tile range fan-out at ~6–8× the design's budget — already absorbed, since Phase 1 types a tile as a **set** of ranges (§5, contracts §2.6); a group-aware merge policy; a re-label landing a row in the wrong group — the `predicate` op is withdrawn, so under decision [0047](../decisions/0047-edit-is-delete-plus-reingest.md) an edit is a delete plus a re-ingest and the new row lands wherever its flush puts it, making placement compaction's problem rather than the deny lane's; and `permutation.bin`'s encoding. That last is the format consequence: the permutation is a flat uncompressed `u32` array because §11.1 deliberately keeps entity order and row order unrelated — spending the entity ordering on term-signature grouping rather than on geometry *(design r22; this parenthesis previously read "leak C6", which is the argument r21 relaxed and r22 removes from load-bearing duty — the ordering is unrelated because it is **spent**, not because a gap on the wire would disclose anything)* — making the values maximum-entropy. Signature-major breaks that — entity IDs are signature-sorted *within each commit window* (design §11.1, [write-path](write-path.md) §2.2; the qualifier matters here, because near-monotonicity is only as good as the window granularity, and the index-ordinal split above is what would make it hold globally), so `entity_to_row` becomes near-monotone within a group and Elias-Fano-class encoding becomes worth having. **Phase 1 must not assume the permutation's representation beyond the contracts spec's reader interface**, or this arrives as a bundle-format break rather than a build flag.

---

## Appendix A — Package manifest

**Rust serving core.** `croaring` (CRoaring FFI, frozen views), `arrow` (arrow-rs), `memmap2`, `rayon` for the parallel mask build, `tokio` with `axum` or `tonic`, `wasmtime` for the plugin sandbox, `rustc-hash` for the term dictionary, `parking_lot`, `serde` with `postcard` for manifests, `tracing` for observability. Dev: `proptest`, `criterion`, `cargo-fuzz`.

**Test-only.** `accumulo-access` (JVM, invoked from CI) as the label-semantics oracle; DuckDB as the mask-build oracle. Neither belongs in a deployed dependency graph. `accumulo-access` is at `1.0.0-beta3` with no GA release, which is acceptable for a test oracle and would not be for a shipped dependency.

**Python (SDK and reference oracle).** `pyarrow` for the SDK's Table returns; `pyroaring`, `numpy` and `polars` in the reference oracle and test fixtures; `hypothesis` for plugin property tests. The clustering pipeline — UMAP, HDBSCAN, Toponymy — is the caller's and out of scope per §2.1; the engine's batch mode consumes its Parquet outputs (2.2).

**Frontend.** deck.gl (MIT) for the GPU profile, Leaflet or OpenLayers (BSD-2) for the thin-client profile, `apache-arrow` for decode, over a shared headless core. *(Written before the client work: the thin-client profile was never built, and the wire identity is now a keyed `tessera_id` rather than a per-session handle — decision 0006. See `client-interaction.md`.)* rather than entity IDs. See the visualisation architecture document.

## Appendix B — Repository layout

A single workspace. The language rule: Python drives, never implements (2.2). The authoritative crate map is the system architecture document's §3; the sketch here shows the shape only.

```
crates/        Rust engine: types, plugin, authz, store, spatial, labels, filter,
               lifecycle, engine, wire, server, build, cli — one binary, serve
               and batch modes (system architecture §3)
python/        the wheel: SDK, supervisor, tessera.build()/tessera.serve() wrappers
reference/     deliberately slow, obviously correct Python — the differential oracle
conformance/   the invariant suite (section 10.2)
  oracles/     accumulo-access (JVM) and DuckDB harnesses — CI only, never shipped
frontend/      shared headless core + per-profile renderer shells
```

`reference/` and `conformance/` are not test utilities filed out of the way. They are the two directories that make the rest of it defensible.

