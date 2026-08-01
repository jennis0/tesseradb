# 0025 — A key rotation invalidates sessions; identifiers are not stable across them

**Date:** 2026-08-01 · **Status:** Settled. The session-token design that implements it is not yet written.

## Context

A `tessera_id` is a keyed permutation of an entity identifier. Rotating the key changes every
identifier in the corpus.

The previous handling made the rotation counter a request parameter: present a stale identifier
with its counter and the service refuses; present it without one and the service resolves it under
the current key, where it is a perfectly valid identifier **for a different item**. That was
recorded as the caller's accepted trade, which nobody had accepted.

It was also a probe. A caller able to vary the counter learns how the mapping moved across a
rotation, which is information about the corpus rather than about any item they hold.

## Decision

**A key rotation is a session invalidation event.** `tessera_id` values are not guaranteed stable
across sessions, and on any given request an identifier is **assumed current** — interpreted under
the live key, with no per-request counter to supply or to vary.

The active **idset** is published on `/v1/meta`, so an external system holding cached identifiers
can check whether they need invalidating. That is a deliberate poll, not a per-request parameter.

## Why this rather than checking

It dissolves the problem instead of detecting it. There is no counter to omit, so there is no
silent wrong answer; there is no counter to vary, so there is no probe. It also makes the contract's
existing framing operative rather than bolted on — `tessera_id` was already specified as a
transport identifier rather than a durable key, and already documented as unstable across a rekey.

It narrows the accepted disclosure of stable wire identity: identity is stable within a session
rather than indefinitely.

## What this obliges

**A rotation must actually end live sessions.** A session that survives one holds identifiers that
now mean something else, which is the original failure relocated rather than fixed.

## The intended mechanism, recorded but not designed

The session token becomes a **JWT** carrying an opaque reference to the materialised visible set,
the idset it was issued under, an issue time, and principal information. Validation checks the
idset against the live one, so a token issued before a rotation fails on presentation and is
handled exactly as an expiry is. No sweep over live sessions is needed.

This is attractive for a reason worth stating: a JWT's usual weakness is that it cannot be revoked
before expiry, and that does not apply here, because the visible set is server-side. Dropping it
revokes immediately, which is what `/session/revoke` already does by a non-capability token id.

**Constraints the design must honour, established when this was ruled:**

1. **The reference to the visible set must be unguessable.** A sequential reference lets a holder
   infer that neighbouring ones exist, which is a count of how many principals have authorised — a
   corpus-wide quantity.
2. **I6 needs amending, and that needs its own review.** It currently reads that the service *never
   infers, looks up or refreshes credentials*. Looking up a visible set by reference is literally a
   lookup. The intent survives — the set was computed from auth data presented at authorisation and
   retrieving it refreshes nothing — but the wording does not, and an invariant amendment is not a
   footnote.
3. **This does not make the service stateless.** The visible set stays server-side; the token is a
   signed pointer plus assertions. Worth stating, because "we use JWTs" is usually taken to mean
   validation needs no server state, and someone will plan horizontal scaling on it.
4. **Expiry derives from `token_max_lifetime`.** That key is required with no default so that a
   deployment must choose it; a token carrying its own independent expiry is a second source of
   truth and the two will drift.
5. **The payload is an enumerated list with a reason per field.** It is the one part of the system
   the viewer plane can read directly, so what goes in it is a disclosure decision rather than a
   convenience one.

## Evidence

Register row A9. `docs/design/contracts.md` §2.6 (the identifier's construction and its stated
instability across a rekey), §3.3 (revocation by token id); architecture §4 for I6 and Appendix C
row C17.
