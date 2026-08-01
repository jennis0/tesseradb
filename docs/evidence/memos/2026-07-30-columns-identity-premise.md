# Compiler-enforced premise check: columns identity premise (2026-07-30)

## Method

Rather than relying on grep or manual audit, the premise was verified by renaming both `ColumnsRef::entity_id()` and `ColumnsRef::node_id()` accessor methods to trigger compiler errors at every call site. A `cargo check --workspace --all-targets` enumerated all consumers; any error report identifies exactly which files and lines use these columns.

## Verified consumer set

The compiler enumeration found exactly four consumers:

| File | Line | Column | Consumer | Purpose |
|------|------|--------|----------|---------|
| `crates/tessera-engine/src/viewport.rs` | 113 | `entity_id()` | **Linear scan (`.iter().position()`) for the row holding a given entity ID** | The *inverse* direction — identity→row — on the `/v1/items` drill-down path |
| `crates/tessera-engine/src/viewport.rs` | 314 | `entity_id()` | Extract entity ID at a row index | Pure row→identity mapping for output serialisation |
| `crates/tessera-store/tests/bundle_read.rs` | 192 | `entity_id()` | Read full entity ID column | Test-only validation of round-trip fidelity |
| `crates/tessera-store/tests/bundle_read.rs` | 195 | `node_id()` | Read full node ID column | Test-only validation of round-trip fidelity |

## Conclusion

Both columns are used exclusively for identity materialisation on the output side, and neither participates in authorisation. `entity_id()` serves two production readers, and they run in **opposite directions**, which matters for the change:

- `viewport.rs:314` is row→identity: it reads the value at a gathered row to put an identity on the wire. Replacing the column's contents is transparent here — the new value flows through unchanged.
- `viewport.rs:113` is identity→row: it linear-scans the column for the row holding a caller-supplied entity ID, on the `/v1/items` drill-down path. This reader is **not** transparent to the change: once the column holds minted identities, a scan for an entity ID would be searching the wrong space. It must be replaced, not merely re-pointed — which is what the plan does by inverting the bijection to recover the entity, testing visibility in entity space, and taking the row from the permutation. Any implementation that leaves a scan of this column in place is a defect, not a simplification.

The `node_id()` column has zero production consumers — it appears only in test assertions that validate the bundle's column data fidelity. No consumer reads either column during mask composition, authorisation evaluation, or any load-bearing computation over the identity space.

The forward permutation (`permutation.bin`) is the source of truth for address translation within mask composition; the columns are never consulted there. The premise therefore holds: **no authorisation decision reads either column, so replacing their contents cannot change what a principal may see.** The change is structurally safe in that sense — with the one caveat recorded above, that the identity→row reader at `viewport.rs:113` is replaced rather than inherited.
