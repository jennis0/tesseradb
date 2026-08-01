# House style

This repo has a consistent and unusual voice. It was never written down, so it has been
learnable only by reading a lot of it, and it drifts every time someone new contributes. This
document is that voice, stated once, with real examples from the tree.

It applies to design documents, memos, commit messages and code comments alike. The medium
changes; the standard does not.

## The standard

**Argue the decision; do not assert it.** A reader who disagrees should be able to find the
reason and attack it. The best example in the tree is `crates/tessera-engine/src/select.rs`,
whose module doc opens a section titled *"Why there is no candidate-list route"* and then spends
fifty lines making the case with evidence — because the alternative is specified in §7.2, looks
obviously better, and will be proposed again by the next reader who has not seen the numbers.

**Record what was rejected.** Rejected alternatives are the most-reread part of a design and the
first thing lost in a rewrite. Someone will propose them again; the document's job is to answer
before they spend a week on it.

**Distinguish measured from modelled, and say which.** Write "measured" only where something was
measured. `clients/ts/core/src/coords.ts` says *"Measured, not assumed"* and names the test that
did it, with the instruction to change the constants only with that test. That is the form.

**State negative results plainly.** `crates/tessera-bench/src/arms/ingest.rs` says
*"F3: NOT confirmed by measurement — do not claim it is."* A refuted hypothesis is a result. This
repo has several optimisations that looked certain and were killed by measurement, and the
record of the killing is what stops them coming back.

**Say what a thing does not do, wherever the scope is contestable.**
`crates/tessera-lifecycle/src/faults.rs` states what its compile gate buys, then states what it
does not buy — *"stated because the first draft of this paragraph claimed otherwise and was
wrong"*. Correcting yourself in the document is not an embarrassment; it is the most useful
sentence on the page, because the wrong version was plausible.

**Lead with the result.** Then the choices, then the reasoning. History goes last, or becomes a
pointer. A memo that opens with a narrative of how the investigation proceeded has buried its
own conclusion.

**Write so a decision can be made.** If you are escalating, the reader must be able to rule
without opening a source file: what is true now, what would change, what the code does in one
sentence, the options with their consequences, and a recommendation with what it costs if wrong.

**British spelling** — authorisation, visualisation, licence, behaviour.

**Use the established security vocabulary**, not invented terms: *conservative label join*,
*boolean expression indexing*, *partial evaluation*, *Non-Truman model*, *compartmented MAC*.
Each brings a literature with it, and a reviewer will find the lineage anyway.

## Code comments specifically

**Module docs carry the design argument, and they are long here on purpose.** Sixty to a hundred
and thirty lines of `//!` is normal in this tree where it is warranted. It is warranted when:

- the module upholds an invariant, and the reasoning for *how* is not visible in the code;
- an obvious construction was rejected for a non-obvious reason;
- a measurement drove the shape, and the shape looks arbitrary without it;
- the module is one of a pair, or duplicates something deliberately, and a reader will otherwise
  "fix" the duplication.

It is not warranted for restating what the code says. Length is a consequence of having
something to say, not a target.

**Comments record settled decisions and evidence — not backlog.** There are essentially no
`TODO`, `FIXME` or `XXX` markers in this repo and that is deliberate. Open work is an issue,
where it can be found, labelled and closed. A `TODO` in a source file is a note to nobody.

**State the rule once, in the place that owns it.** `crates/tessera-engine/src/pins.rs` has a
heading *"The rule, stated once"* and then annotates every guard in the file with which half of
that rule it defends. Repeating a rule in five places means five places to update and four
chances to disagree with yourself.

**Cite the governing section, and prefer stable citations.** `§4`, `contracts §2.5`,
`lifecycle §3` are stable. `file.rs:184` is not — it drifts on any edit above the cited line, and
this repo has already had line references go stale across a merge and strand a worker mid-task.
Use `file:line` only when nothing else identifies the thing, and expect it to rot.

**State load-bearing assumptions at the site.** Not in your head, not in the commit message. If
the assumption is load-bearing, make it a test; if it cannot be a test, make it an assertion; if
it can be neither, say so and say why.

**Admit when a previous claim was wrong.** `crates/tessera-authz/src/single_flight.rs` says
*"That claim was made before it was true, which is why the table below exists"* and then gives
the coverage table that makes it true. That is worth more than a silent correction, because the
next reader would otherwise trust the original claim for the same reasons you did.

## Commit messages

Say what changed and why it was worth changing. Where a change is invariant-bearing, say which
invariant and how you know it still holds. Where you deliberately did not do something, say so —
the reader's next question is usually "why didn't you also…".

## The test

Before you commit prose, ask: **could a competent reader who disagrees with this find the
argument and attack it?** If not, you have asserted rather than argued, and the next person to
touch this will either obey it without understanding or ignore it without knowing what they
broke.
