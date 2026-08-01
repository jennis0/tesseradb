# 0031 — The decode tiers are described where selection is specified

**Date:** 2026-08-01 · **Status:** Settled

## Context

Leak-register row C19 **accepts** a disclosure: per-tile selection work varies with the viewer's own
visible count, widened to cover a decode mechanism chosen per tile from three tiers.

The argument for accepting it is that both quantities gating the choice are ones the viewer already
holds exactly — the per-tile visible count is disclosed by the response, and the tile grid is
public — so the timing variance reveals nothing beyond the response body.

The tiers themselves were defined only in the engine's source. An assurer reading C19 was being
asked to accept a residual they could not evaluate.

## Decision

**Describe the three tiers where the selection definition is specified**, so that C19's argument can
be checked against them:

- the whole range visible, so the visible set *is* the range and no bitmap decode happens;
- density at or above a threshold, decoded as contiguous runs;
- otherwise, batched value decode, whose cost is flat where the run tier's collapses.

**Marked the same way as [0030](0030-determinism-is-not-a-guarantee.md): a documented implementation
detail that may change, not a contract.** Describing a mechanism so a disclosure argument can be
audited is not the same as promising it, and the distinction should be explicit — otherwise the next
person to improve the decode believes they are breaking a published contract.

## Why not in the register row

Leak-register cells state what leaks, its severity and its mitigation. They are not where mechanisms
are described, and a row that has to carry a mechanism in order to stand up is a sign the mechanism
is missing from the specification.

## Evidence

Register row A8. Leak register C19. `crates/tessera-engine/src/select.rs` — `DecodeTier` and its
gating; `crates/tessera-engine/examples/decode_tiers.rs` — the re-runnable evidence.
