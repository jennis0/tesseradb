# 0017 — C4 covers published timing as well as inferable timing

**Date:** 2026-08-01 · **Status:** Settled

## Context

`/v1/viewport` emits three timing headers. `x-tessera-server-us` and `x-tessera-admission-us` are
unconditional and the shipped TypeScript client reads both. `x-tessera-stage-ns` is double-gated —
on a build feature and a config key — and carries per-stage row counters.

Leak-register row C4 covers response timing as an **inferable** channel: viewport service time
varies with the row span a tile covers, which includes items the viewer cannot see. It is Open and
unmitigated. The headers publish that same channel **explicitly**, at microsecond resolution, per
request, to anyone holding a session token — and they were documented as contract without a
register row.

## Decision

**C4 is rescoped to cover both forms.** Timing is disclosed implicitly, through service time
varying with tile row span, and explicitly, through published measurements on the viewer plane.
One channel, one row.

**`x-tessera-stage-ns` is diagnostics only.** Its per-stage row counters are volumes, not
latencies — a different and coarser quantity than the other two headers publish. It stays off any
session-token surface by default, which is what its double gate already achieves.

## Why one row rather than two

The register's value depends partly on being short enough to read in full. An implicit side channel
and a documented API field are different to reason about, but they are the same disclosure: how
long the server spent, which correlates with how much unauthorised data it walked. Splitting them
would invite the reading that closing one closes the channel.

## What does not change

C4 stays **Open** and **unmitigated**. Publishing the channel deliberately does not quantify it,
and the argument that the disclosing component is the residual after correlation with the viewer's
own visible count is analysis, not measurement. No run isolates the span component.

## Evidence

Loss-detection review of contracts r12. `crates/tessera-server/src/viewer.rs`;
`clients/ts/core/src/client.ts`. Leak register C4 in `../design/architecture.md`.
