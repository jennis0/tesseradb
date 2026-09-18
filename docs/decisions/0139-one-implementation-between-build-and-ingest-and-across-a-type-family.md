# 0139 — One implementation between build and ingest, and across a type family

**Date:** 2026-09-11 · **Status:** Settled (owner ruling) · Not built: applied by the pass in
[`../evidence/memos/2026-09-11-duplication-and-consistency-pass.md`](../evidence/memos/2026-09-11-duplication-and-consistency-pass.md).
Extends [decision 0091](0091-build-is-ingest-into-an-empty-database.md) from what the two entry
points mean to how they are written.

## What this answers

Decision 0091 says a build and an ingest of one corpus are the same database to every client.
The tree met that rule with two spellings of each rule and each transformation, the engine's copy
transcribed from the build's with a comment saying the two must agree. Twelve such pairs have
stopped agreeing, and each disagreement is a statement one entry point accepts and the other
refuses. The same shape repeats across the scalar type family: seven copies of the family list,
one per crate, each with its own subset and payload.

## The decision

**Build and ingest share one implementation of each rule and each transformation.** A validator,
a placement predicate, a width table, a code conversion, a dictionary writer or an index writer
exists once, in the lowest crate both entry points can see, and both call it. The entry points
may differ in scheduling and acquisition, as decision 0091 allows. They may not differ in a rule.

**A type family is handled by one implementation over its members.** The scalar family has one
declared list; a table over the family is generated from it; a value or column enum over the
family exists once per storage representation and no more.

**A second copy is an exception.** It is argued at the item, in the code or in the memo that
introduces it, with the reason it cannot be one implementation. Nothing is assumed to be an
exception. The reasons accepted so far: a dispatch whose exhaustive match is a compile-time check
at each consumer; a test oracle that is a second reader of an artifact; a test that sees a layer
the other test does not.

**The survivor of a pair is the more efficient copy, with every correction ported.** Where two
copies exist, the one kept is the one with the better memory, CPU and disk behaviour. Every
correction the deleted copy carries and the survivor lacks is ported to the survivor with its test.
A test that covers a rule only the deleted copy's tests covered is moved, not dropped.

## Consequences

- A new rule or transformation is written once, below both entry points, or it is unfinished.
- A comment that says two copies "must agree" names a defect. The comment is replaced by the one
  copy it describes.
- A refusal that exists at one entry point and not the other is a bug at the entry point that
  lacks it, unless it is about acquisition. The register in the memo above lists the current
  ones and the direction each takes.
- The layering scripts already permit this: `scripts/check-layers.sh` denies named edges, and a
  shared implementation moves down into `tessera-types`, `tessera-spatial`, `tessera-store`,
  `tessera-filter` or `tessera-analyse`, never sideways into the engine. A leaf crate added for a
  shared piece is permitted by the same rule.
