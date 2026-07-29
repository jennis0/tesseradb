# Compiler-enforced premise check: columns identity premise (2026-07-30)

## Method

Rather than relying on grep or manual audit, the premise was verified by renaming both `ColumnsRef::entity_id()` and `ColumnsRef::node_id()` accessor methods to trigger compiler errors at every call site. A `cargo check --workspace --all-targets` enumerated all consumers; any error report identifies exactly which files and lines use these columns.

## Verified consumer set

The compiler enumeration found exactly four consumers:

| File | Line | Column | Consumer | Purpose |
|------|------|--------|----------|---------|
| `crates/tessera-engine/src/viewport.rs` | 113 | `entity_id()` | Binary search for a row by entity ID | Pure row→identity lookup for request routing |
| `crates/tessera-engine/src/viewport.rs` | 314 | `entity_id()` | Extract entity ID at a row index | Pure row→identity mapping for output serialisation |
| `crates/tessera-store/tests/bundle_read.rs` | 192 | `entity_id()` | Read full entity ID column | Test-only validation of round-trip fidelity |
| `crates/tessera-store/tests/bundle_read.rs` | 195 | `node_id()` | Read full node ID column | Test-only validation of round-trip fidelity |

## Conclusion

Both columns are used exclusively for output-identity materialisation. `entity_id()` serves two production readers, both of which perform pure row→identity transformations with no authorisation logic: one locates rows by entity ID during viewport request handling; the other produces entity IDs for output.

The `node_id()` column has zero production consumers — it appears only in test assertions that validate the bundle's column data fidelity. No consumer reads either column during mask composition, authorisation evaluation, or any load-bearing computation over the identity space.

The forward permutation (`permutation.bin`) is the source-of-truth for address translation within mask composition; the columns are never consulted. Therefore, replacing `entity_id` and `node_id`'s contents with minted identities from the identity allocator is structurally clean — the new values will flow through the existing readers unchanged, reaching output with the same type safety and serialisation contract, while the identity assignments remain under cryptographic and audit control.
