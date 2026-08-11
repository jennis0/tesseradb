# 0061 — I13a forbids undetectable or incorrect partials, not streamed truncation

**Date:** 2026-08-11 · **Status:** Settled (owner ruling)

## Context

Invariant I13a's headline sentence — *a request that fails or is cancelled yields no partial
answer* — was literally true under the one-shot viewport response: the client got everything or
nothing. The streamed response (`streamed-serving.md`) cannot satisfy the literal reading: a
disconnect, a shed, or a mid-transfer fault leaves the requesting client holding the frames
already delivered. That is inherent to any streaming protocol, so either the invariant's meaning
is stated precisely enough to cover it, or streaming is off the table.

## The decision

**I13a's "no partial answer" means no partial that is *undetectable* or *incorrect* — and the
streamed response satisfies it.** The annotation at the invariant (architecture §4) is ratified:

1. A truncated stream is **client-detectable by construction**: the trailer frame never arrived
   (contracts §3.2 r26's completeness rule), and a mid-stream server fault additionally aborts
   the connection. Nothing partial can be mistaken for complete.
2. Everything delivered is **exact**: computed against one generation snapshot, counts precise,
   and each tile's points an ascending-`tessera_id` prefix of its served set — precisely the
   subset `delta-serving.md` §7 licenses a client to draw. Partial means incomplete, never wrong.
3. The shared-work clauses are untouched: no *other* request can observe another request's
   partial state, and no failure is ever cached.

A change that made a truncated stream indistinguishable from a complete response would violate
I13a as ratified.

## Why

The invariant's purpose — traced through its own elaboration and its tests — was always the two
observable harms: a partial *presented as* complete, and shared state read half-built. The
one-shot transport made all-or-nothing delivery a free side effect; it was never the protected
property. Reading it as literal all-or-nothing would forbid the streamed response outright and
buy nothing the trailer rule does not already guarantee.

## What was rejected

**The literal reading** — all-or-nothing delivery as invariant. It reverts the viewport to a
buffered monolith, which is the measured client-side defect (19–42 MB arrivals, seconds of
hitching) the streaming work exists to remove.
