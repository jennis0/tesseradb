# 0004 — The external-ID sidecar is transitional

**Date:** 2026-07-29 · **Status:** Settled, transitional

## Decision

The external → internal sidecar is **transitional**, and carries three inherited conditions:

1. Failures are fail-closed typed errors. A `None` must never read as "no external ID".
2. It stays off the request path.
3. Its integrity is verified before any answer leaves.

There is deliberately **no** `tessera_id → entity` direction.

## Why

A lookup structure keyed by an identifier the client supplies is exactly the shape that leaks if
it is allowed onto the hot path or allowed to answer ambiguously. Marking it transitional keeps
the constraint attached to it rather than letting the structure become load-bearing by default.

The distinction that makes it acceptable: a cold metadata store read only *after* the visibility
test has returned visible is a different question from one consulted to decide visibility.

## Evidence

Owner ruling, mirrored verbatim in `crates/tessera-store/src/sidecar.rs`, and enforced
structurally — `mod sidecar` is private and only `ExternalIdSidecar` is re-exported. Contracts
§2.4.
