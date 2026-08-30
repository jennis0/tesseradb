"""The independent Python differential oracle (Task 14).

Deliberately slow, obviously correct, independently derived (plan §10.3). It re-implements the
*definitions* — quantisation (R2), priority (R3), mask = set union from ``pairs.parquet``, count
= brute-force loop — from scratch. It must never call into the Rust engine or share logic with
it; it only reads bundle files directly and talks to a running server over HTTP (Python is a
consumer, never a component — CLAUDE.md "Working method").

## Three kinds of thing live here, and knowing which is which is the review instruction

The package name says "oracle", and only the first group below is one. The distinction is not
cosmetic: it decides what a reader is entitled to assume when they open a module, and what a
change to it can break.

**Definitional — pure, no I/O beyond reading a bundle, no server.** ``viewport`` (§7.2 written as
a definition), ``mask`` (I1's composition), ``filters`` (decision 0062's tree as a per-entity
walk), ``text`` (records §10's predicates as set-and-sequence arithmetic), ``morton``,
``identity``, ``bundle``, ``wire``, ``record_blob`` (the blob's addressing walk — structure only,
never values; its module doc states the records §3/B7 licence it holds to). These are the second
implementation of record. They are held to being *obviously the spec*: literal constructions, no
cleverness, and a documented pin to the design revision they were checked against. A differential
is only as good as these are independent, so a change here that imports an engine assumption
silently weakens every test in both suites.

``text`` is in this group despite running a subprocess. It holds no dictionary and no ordinals,
and the one thing it asks the shipped binary for is UAX #29 segmentation, on decision 0070's
argument that a second ICU implementation would make every marginal disagreement a research
question rather than a defect; the set-and-sequence arithmetic actually under test is its own. The
echo is declared at its own module doc, which is the condition on which it stays here.

**Fixture builders — they synthesise a corpus and shell out to the CLI.** ``catalogue``,
``canary_fixture``, ``label_fixture`` (I3's containment corpus, built backwards from the property's
edge). They produce inputs; they assert nothing about the system. Their failure mode is a fixture
that has quietly stopped being the shape it claims, which is why the first two carry a
``verify()``-shaped re-derivation and why reuse is decided by a stamped recipe. ``label_fixture``
carries none: its one property — the two principals being exactly one entity apart — is asserted
from the engine's own masked counts in the test that uses it, before anything rests on it.

**Drivers — they talk to, and *mutate*, the system under test.** ``harness`` (spawns
``tessera serve``), ``journal`` (drives the control plane and records what was acked). These are
the only modules that can change the thing being measured, and the only ones where "what did we
actually ask for, and what did it say?" is a question worth a type — which is what ``AckedJournal``
is.

Kept as one package rather than split into three, on the argument that the split is a rename of
every import in both suites for a boundary the module names already carry — and that the boundary
worth enforcing (the definitional group importing nothing from the driver group) is a property to
check, not a directory to create. ``test_oracle_layering.py`` is that check, and it derives the
package's module set from the directory rather than from a list: a module this taxonomy does not
place fails it, so the prose above cannot fall behind the package without saying so — which it had,
for ``text`` and ``label_fixture``, until 2026-08-30. Revisit if a fourth kind appears, or if a
definitional module ever needs a driver: that would be the real signal, and this note is here so
the next reader recognises it as one.
"""
