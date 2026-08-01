# 0007 — *K*<sub>max</sub> raised from 128 to 500

**Date:** 2026-07-30 · **Status:** Settled

## Decision

The per-tile mark ceiling `k_max_marks` is **500**, not 128. The selection window is 250.

## Why

Measured against the transport and render path rather than assumed. The probe campaign is the
evidence; 128 was a placeholder that predated it.

## Note

The overplot ceiling and the machine ceiling are deliberately **not** the same knob, so that
raising the machine ceiling on transport evidence cannot silently dissolve the cap clause.

## Evidence

`crates/tessera-server/src/config.rs` records the decision and its date at the constant. Probe
campaign in [`../../probes/`](../../probes/) and the archived drawn-mark-budget plan.
