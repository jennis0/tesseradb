# 0005 — `tessera_id` is a keyed bijection, not a 128-bit random

**Date:** 2026-07-29 · **Status:** Settled · **Refines:** [0003](0003-external-id-as-boundary-identity.md)

## Decision

The boundary identifier is a minted ID: a keyed bijection over `(shard, entity)`, rather than a
128-bit random value.

## Why

The bijection is collision-free by construction, dissolves a leak-register entry, and removes one
direction of the sidecar entirely.

The cost is that the reference oracle must reproduce the construction — which is why the
construction is specified byte-exactly rather than described.

The alternative, a 128-bit random, needed no such reproduction but put the hot file at roughly
25 GiB, worse than the position it replaced.

## What it is not

A cryptographic guarantee. See [0014](0014-i10-weakened-to-construction.md).

## Evidence

Normative construction in
[`../evidence/memos/2026-07-30-tessera-id-construction.md`](../evidence/memos/2026-07-30-tessera-id-construction.md),
implemented in `crates/tessera-types/src/identity.rs`, with test vectors at
`reference/vectors/tessera_id.json`. Contracts §2.6.
