# 0133 — A null access label takes the view's default at both doors, or is refused at both

**Date:** 2026-09-06 · **Status:** Settled (owner ruling) · ⊘ not yet built

## What this answers

The build fills a row whose access label is null or empty with the view's
`point_visibility.default` (`tessera-build/src/input.rs`); the ingest wire applies no default, so
the same row ingested is visible to nobody. The difference narrows rather than widens, and no
current corpus has such a row, but decision 0091 says the two entry points read one corpus the
same way, and here they do not. Found by the wire track's referee, 2026-09-05.

Three options were put: ingest applies the default too; the build stops defaulting and a null
label is refused at both doors; leave it documented. A reserved label meaning open visibility was
raised beside them and is deferred to its own change.

## The decision

**`point_visibility.default` becomes optional. A view that declares one applies it to a null or
empty label at both entry points; a view that declares none refuses a null or empty label at
both, with the count of rows refused.** The fill is a declared visibility decision, made once in
the declaration and applied the same way by the build and by `/control/ingest`; a fill nobody
declared is not made anywhere.

## Why

A default label is a visibility decision. Made silently by a build it is a rule the operator never
wrote; made by the declaration it is one the operator owns and both doors honour. Refusing where
none is declared is fail-closed in the direction the rest of the system takes, and reporting the
count is what lets an operator decide whether to declare one.

## Consequences

- `configuration.md`'s `point_visibility` row: `default` optional; `views.md` where the field is
  described.
- The build refuses a corpus with null labels and no declared default, naming the count; a corpus
  that relied on the fill declares the default it was getting.
- `/control/ingest` applies a declared default to an empty label list; an empty list on a view
  with no default is a refusal for the batch, in the same terms the build uses.
- The reserved open-visibility label is not part of this decision.
