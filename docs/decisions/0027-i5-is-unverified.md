# 0027 — I5 is unverified, and the specification says so

**Date:** 2026-08-01 · **Status:** Settled

## Context

I5 requires the two authorisation functions to agree about what a term means: if the data function
indexes item *i* under term *T*, then every principal for whom the auth function yields *T* must be
authorised for *i*.

The specification is blunt that everything downstream rests on this and none of it can check it —
masks, label containment and partitioning all assume the agreement holds. It then claimed a
mitigation: for the label half, an oracle *already exists rather than needing to be written*, in
the form of a specification-conformant external implementation of the same access-expression
grammar, run as a differential.

That oracle does not exist here. There is no such dependency, no access-expression parser, no
k-of-N evaluation, and nothing to run them with. Nor is there anything to check even in principle:
the only authorisation plugin is a passthrough for which both functions are the same string
comparison, so I5 is trivially true and no differential can disagree.

## Decision

**Remove the mitigation claim. State that I5 is unverified.**

I5 rests on the caller's semantic obligation, and nothing independently checks it. That is the
accurate description of today, and the specification should say it in the section that raises the
problem rather than pointing at a mitigation that is not there.

## Why this rather than a plan

A stated plan that nothing is executing reads as coverage. The gap here is the system's largest
unverifiable dependency, and an assurer needs to see it rather than see a reference to a tool that
was never brought in. When there is something to verify — a plugin whose two functions can
genuinely disagree — the question of what verifies it can be answered then, on the evidence
available at that point.

## What this does not decide

How I5 eventually gets checked. Two routes remain open and neither is chosen here: an external
implementation of the grammar, whose independence is the argument for it; or a second
implementation written alongside the suite, which is the pattern the mask differential already uses
successfully. Both need a non-trivial plugin to exist first.

## Evidence

Register row S8. Architecture §6.1 and Appendix E. `crates/tessera-plugin/src/lib.rs` — the
passthrough plugin. `reference/oracle/mask.py` — the in-repo second-implementation pattern, for
comparison.
