# 0135 — A generating set is the caller's claim; I8 is withdrawn

**Date:** 2026-09-07 · **Status:** Settled (owner ruling) · ⊘ the mechanism for changing a set at
ingest lands with the ingest design under decision 0134; the invariant text is withdrawn now

## What this answers

Invariant I8 said a label's generating set is immutable once supplied: items arriving later must
not be added, and a shrink was given narrowly (r42, register row C7) to a layer declared
permissive. Under decision 0134 every part of an artifact must be creatable and extendable at
ingest, and a generating set was the one part with a rule against it. The owner's reading: what a
content was derived from is the caller's claim about their own data, not a decision the service
should own.

## The decision

**I8 is withdrawn. A generating set is the caller's claim, changeable at ingest in either direction
like a membership.** The service evaluates containment against the set as currently declared,
inside the principal's composed mask — I2 and I3 unchanged — and trusts a declared set exactly as
it already does at supply (Appendix C, C12). The strict and permissive layer modes are no longer a
service invariant: a caller who wants a set never to shrink keeps it so; the fold's removal of a
deleted member under a permissive layer stands as a convenience the caller declared.

## Why

The service never verified a generating set; I8 only refused to let one change. Growing a set was
never a disclosure question — content is then served to fewer principals, not more — and its ban
was about the honesty of a derivation claim the service cannot check. Shrinking has a disclosure
consequence, content derived from an item reaching a principal who cannot see it, and the register
already carried that as the caller's declaration (C7). Withdrawing I8 makes the register say the
true thing once: the declared set is what is served against, and the caller owns it.

## Consequences

- `architecture.md` §4: I8 withdrawn with a pointer here; §7.6 and §12's forbidden-operation line
  amended; `inventory.md`'s invariant row. Appendix C: C7's disposition becomes "the declared set,
  whatever the caller did to it"; C12 unchanged in kind and now the whole of the story.
- `annotation-write-cycle.md` rests on I8 in several places (§2.1's modes, the `G` change rule,
  the per-event tables); it is marked at its head and rewritten with the ingest design rather than
  piecemeal.
- The ingest design (decision 0134) gives a generating set the same unit of ingest as a membership;
  a multi-part upload is not required by I8, since I8 is gone.
