# 0020 — No authorisation data is retained beside a mask

**Date:** 2026-08-01 · **Status:** Settled

## Context

The specification said eviction must be transparent, and that **the authorisation data is retained
alongside the mask** so that a mask evicted under memory pressure can be rebuilt rather than
failing at an arbitrary moment.

The implementation does not do this, and has not. Transparency comes from two other things: a live
session holds its frozen fragment directly, and there is a digest-verified on-disk fragment cache
that survives restarts. No authorisation data is kept beside a mask anywhere.

## Decision

**The implementation is right and the specification was wrong.** No authorisation data is retained
alongside a mask. Eviction transparency is provided by the live fragment reference and the
digest-verified on-disk cache.

## Why

Retaining credentials beside a mask is a security cost — it extends the lifetime of authorisation
material beyond the request that presented it, and puts it in a structure whose eviction policy is
tuned for performance. The specification was accepting that cost to buy a property the code
achieves another way, so it was paying for nothing.

This is one of the places where the implementation is *stronger* than the document specified, which
is worth recording explicitly: the usual direction of drift is the other way, and an unrecorded
improvement is as likely to be "corrected" back as an unrecorded defect is to persist.

## Evidence

Register row S4. `crates/tessera-authz/src/fragment.rs` (the on-disk cache, digest-verified before
the frozen view is constructed); `crates/tessera-engine/src/session.rs` (the live reference).
