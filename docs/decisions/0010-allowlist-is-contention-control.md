# 0010 — The track allowlist is contention control, not a design constraint

**Date:** 2026-08-01 · **Status:** Settled · **Supersedes:** an earlier controller note that concluded the opposite

## Decision

**If the right design is prevented by the file-ownership allowlist, that is a problem to fix, not
to swallow.** A worker that needs a file outside its allowlist says so and it gets granted. The
allowlist exists to stop parallel tracks colliding, not to shape designs.

## Why

The first draft of a task brief recorded a config file as unowned, concluded the task therefore
could not add a config key, and instructed the worker to derive every operand of its arithmetic
from keys already landed. That is the failure mode: **a worker who reads an ownership map as a
design constraint will silently produce the second-best design and report it as done.**

The correction is recorded in the brief itself rather than edited out, because the mistake is more
instructive than the fix.

## How it is applied

Briefs now carry the standard explicitly: *the allowlist is contention control, not a design
constraint — if the right design needs a file, say so and it gets granted.*

This does **not** weaken the rule that a worker may not edit the allowlist to make a check pass.
Granting is a controller decision, recorded with its reason.

## Evidence

Owner correction in the stage 2.1 ledger, and the controller ruling that implemented it.
[`../agents/parallel-work.md`](../agents/parallel-work.md).
