# 0003 — External ID becomes the boundary identity

**Date:** 2026-07-29 · **Status:** Settled, and refined by [0005](0005-tessera-id-keyed-bijection.md)

## Decision

The caller's own identifier becomes the identity at the trust boundary. Internal → external is
served by the columns; external → internal moves to an off-hot-path sidecar. The `node_id` column
is dropped. Taken into Phase 1 scope rather than deferred.

## Why

Taken mid-phase because the alternative was measuring a format that was about to be replaced. The
conformance and differential work against the 10⁹ server was deferred until after the change
landed and the bundle was rebuilt.

## Consequence

An entity ID never needs to leave the engine for a client to name an item, which is what makes
I10 structural rather than a filter: no request-path artifact stores an entity ID, so the gather
cannot produce one.

## Evidence

Owner decision recorded in the Phase 1 ledger. Leak register C6 revised in the same change.
