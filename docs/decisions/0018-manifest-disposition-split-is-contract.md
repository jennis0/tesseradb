# 0018 — The side-manifest disposition split is interchange contract

**Date:** 2026-08-01 · **Status:** Settled

## Context

When a replica walks `SEGMENTS-<n>.json` candidates and one fails to verify, what it does next
depends on what that manifest carried. A manifest carrying only `deltas` may be stepped past —
items go *missing*, never re-exposed. A manifest carrying `tombstones` or `deny` may not: stepping
past it serves an older manifest that predates the deny, undoing an accepted suppression
indefinitely and with no operator signal.

This was an implementation property of `tessera-store`. Contracts r12 promoted it into §2.3.

## Decision

**It belongs in the contract.** The rule binds every reader, including the Python oracle and any
future engine.

## Why

An interchange contract exists to make independent implementations agree where disagreement is
costly. A second reader that steps down past a deny-carrying manifest re-exposes suppressed items —
which is precisely the class of failure the corpus treats as fail-open and non-negotiable. Leaving
the rule as one implementation's habit means the next implementation has to rediscover it, and the
cost of not rediscovering it is silent.

## The correction that came with it

Contracts r12's Appendix R claimed the revision made no format or behaviour change and that every
edit merely marked a claim or recorded a field. That was false of this one edit, and an independent
review caught it. The entry now records the addition as an addition.

The rule is also stated with its three residuals rather than without them: a deny-carrying manifest
is still stepped past when it does not parse, cannot be read, or sits under a non-canonical name.
Stating the guarantee without them would have reintroduced the same defect the revision was fixing —
a fail-closed gate described in the present tense.

## Evidence

Loss-detection review of contracts r12. `crates/tessera-store/src/read.rs`. Contracts §2.3.
