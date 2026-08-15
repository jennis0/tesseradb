# 0071 — Fault injection reaches a served binary by its own build, not by a runtime switch

**Ruled:** 2026-08-15, owner.
**Bears on:** [`correctness-suite.md`](../design/correctness-suite.md) §10.1, §12.3, §13, §14;
`scripts/check-layers.sh` rule 2; `tessera-lifecycle::faults`.

## The question

Crash atomicity — a stage killed mid-flight must land on one of its two endpoints and never between
— needs the write path paused at a publication seam, because an arbitrary kill essentially never
falls on one. The pause sites belong in the fault switchboard that already exists.

That switchboard is enabled only through a self dev-dependency. `cargo build` does not build
dev-dependencies, so nothing it produces can carry the code, and `check-layers.sh` rule 2 asserts
the property from the resolved feature graph rather than from manifest text. The correctness
suite's driver boots the ordinary release binary, which by construction has no pause sites in it.
So the suite cannot reach them, and closing that gap moves a fail-closed guard.

## The ruling

**The feature becomes declarable on `tessera-server` and `tessera-cli`, and a binary built with it
goes to its own path.** The default-features build is unchanged and is what every deployment gets.

The guard narrows from *"no normal dependency edge anywhere enables fault injection"* to *"no
default-features release build reaches it"*. It stays mechanical: the question is still answered by
the resolved feature graph, and the rule still fails on the edge it was written to catch. What it
gains is a feature-flag argument, which is why it must be rewritten deliberately rather than
relaxed by exempting a manifest — the failure mode rule 2's own comment records, where exempting
manifest text left the rule green while the switchboard reached the release binary.

## Why not the alternatives

**A runtime-gated debug surface in the shipped binary** — the pause sites compiled in, inert unless
a config key enables them — keeps the compile-time guard untouched and needs no second build. It
was declined because it puts a *stop the write executor here* switch into production, reachable by
anyone who can write configuration. On a system whose proposition is access control, that is a
denial-of-service surface accepted in exchange for harness convenience, and the guarantee worth
having is that the code is absent rather than dormant.

**Keeping crash coverage in Rust**, re-executing the test binary as a child and killing it — what
the WAL crash tests already do — needs no gating change at all. It was declined because the stage
plan, the entitlements and the canonicalisation are the Python suite's, so a Rust crash harness
either reimplements the comparison or asserts something weaker than every other stage. Two
implementations of one comparison is the drift this suite exists to prevent one level down, and
paying it here would be spending the currency the project has decided not to spend.

## What this does not license

The fidelity rule is unchanged and is inherited rather than restated: **an injected failure must be
indistinguishable from a real one, in variant and in order.** A pause site parks a thread holding
no lock. And the seam sites extend the existing switchboard — a second pause mechanism beside the
first is the outcome `conformance.md` §5 has flagged in both directions since r1, the two designs
sharing no vocabulary.

Feature unification is a constraint on how the guard is rewritten, not a reason against the route:
features are additive across a workspace build, and `cargo test --workspace` already unifies this
one into `tessera-server` today. The rule guards the release path, which is the one that matters,
and it must keep saying so explicitly.
