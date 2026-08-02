# How to write here

This governs everything in `docs/` and every code comment. The audience for the design corpus is
a competent technical reader who has not worked on Tessera: an assurer checking the security
argument, an engineer evaluating the approach, a contributor arriving cold. Write for them.

## The six rules

### 1. Describe the system, not its construction

Write what the system **is** and **why it is that way**. Not how it came to be that way.

A reader wants to know that deletion denies retire against a stamp ledger and suppressions never
retire at all, and why those differ. They do not want to know that this was revision 3's
correction to revision 1, that it was found in review, or which phase built it.

On the rare occasions that history is worth keeping, it belongs in `docs/decisions/` and in git — not
interleaved with the explanation. A document whose paragraphs are half archaeology forces every
reader to separate the two, every time.

This applies to the revision trail too. Appendix R exists so a reviewer can see what was already
attacked; it is a few lines per revision, collapsing as revisions age. A design document that is
mostly its own history has inverted the ratio and needs the history cut, not the design expanded.

> ✗ "r3 replaced r1's blanket retirement stamp, because for suppressions that was fail-open. §5.1
> was then amended in r4 once flush became the visibility mechanism."
>
> ✓ "A suppression retires only when it is lifted. Assigning it a retirement stamp — as deletion
> denies have — would eventually expire the entry and make the item visible again."

The second sentence keeps the whole reason. It just does not narrate.

Corollary: avoid phase numbers, task numbers, and "currently"/"for now" in the corpus. If
something is not built yet, mark it — see below — rather than writing in a present tense that
quietly means "eventually".

#### Marking what is specified but not built

The corpus specifies a target; the code is behind it in places. Present tense about absent
machinery is the most damaging error available here, because on a security property it reads as
an assurance. Mark it **at the claim**, never only in a preamble the reader has forgotten by §5:

> **⊘ Specified, not implemented.** One sentence on what exists instead, and what the reader must
> not assume meanwhile.

Rules for the marker:

- It goes immediately after the claim it qualifies — same paragraph or the line below.
- It says what *is* true now. "Not implemented" alone leaves the reader unable to reason about
  the system they actually have.
- Where the unbuilt thing is a **guarantee**, it also says what the current behaviour is instead,
  and whether that is safe. "Safe today only because nothing retires at all" is the useful form.
- Every marker is counted per document in `docs/design/inventory.md`, which is generated, so the set is countable without anyone maintaining a list by hand.

Use **⊘ Partially implemented** where some of a mechanism exists, and name which part.

### 2. A paragraph should be readable on its own

A reader should not need three other documents open to parse one paragraph.

- **Expand an acronym or a project term on first use in each document.** Not once across the
  corpus — once per document. `M_auth`, `I7`, `C4`, DNF, LOD, *conservative label join*: each
  gets a clause the first time it appears, even though it is defined properly elsewhere.
- **Say what a cross-reference is for.** "See §7.2" tells the reader nothing about whether they
  need to go. "The alternative route, and why it was rejected, is in §7.2" tells them.
- Cross-references support the text; they do not substitute for it.

### 3. Calibrate density, and length

Two failures, equally common. **Padding**: restating the heading, narrating the structure
("in this section we will…"), or three sentences where the second was the point. **Compression**:
a paragraph so dense the reader has to decompress it — clauses stacked with em-dashes, three
distinct claims sharing one sentence.

The test is whether a competent reader gets it at reading speed, once.

Length is governed by the same test at document scale. Cover the substance and stop. A document
does not need an introduction restating its title, a summary restating its body, a section for
every heading a similar document happened to have, or an enumeration of alternatives nobody
proposed. Most design documents here are a few hundred lines; the two that run past a thousand
earned it by specifying the whole system, and are not a model to imitate. If a draft has grown
past what it needs, the fix is to cut it before review, not to explain the length.

### 4. Proportion

Emphasis is a budget. If everything is load-bearing, critical and non-negotiable, the reader
cannot tell which things actually are — and this corpus has a small number of things that
genuinely are.

- Reserve **bold** for the sentence a skimming reader must not miss. A paragraph with four bold
  phrases has none.
- Do not call a reasonable design choice groundbreaking, novel, or the key insight. Say what it
  does and let it be judged.
- Scale the space to the importance. A mechanism that took a week to get right but is
  three lines of consequence gets three lines.

### 5. Use a diagram when it beats prose

Reach for one when the subject is a **structure, a flow, or a state machine** — anything the
reader would otherwise reconstruct in their head from a paragraph. Data layout, request paths,
the generation/pin lifecycle, mask composition, the retirement rules: all clearer drawn.

Mermaid, in a fenced ```mermaid block, so it stays diffable and renders on GitHub. Give it a
caption saying what it shows. A diagram that duplicates an adjacent paragraph is padding; a
diagram that replaces one is the point.

Tables count. A three-way comparison is a table, not three paragraphs.

### 6. Say what is true, precisely

- **Distinguish measured from modelled from assumed**, and say which. `clients/ts/core/src/coords.ts`
  writes "Measured, not assumed" and names the test that measured it. That is the form.
- **Record negative results.** `crates/tessera-bench/src/arms/ingest.rs` says
  *"F3: NOT confirmed by measurement — do not claim it is."* A refuted hypothesis is a result,
  and it stops the idea coming back.
- **Distinguish specified from implemented.** Where the corpus describes something not yet built,
  it must say so at that point — not in a preamble the reader has forgotten by §5. Present tense
  about absent machinery is the most damaging error available here, because it reads as an
  assurance.
- **Argue, do not assert.** A reader who disagrees should be able to find the reason and attack
  it. `crates/tessera-engine/src/select.rs` spends fifty lines on "why there is no candidate-list
  route" because the rejected alternative looks obviously better and will be proposed again.
- **Say what something deliberately does not do**, wherever the scope is contestable.
- **British spelling**: authorisation, visualisation, licence, behaviour.
- **Use the established security vocabulary** rather than inventing terms: *conservative label
  join*, *boolean expression indexing*, *partial evaluation*, *Non-Truman model*,
  *compartmented MAC*. Each carries a literature a reviewer will find anyway.

## Code comments

The same rules, plus:

**Module docs carry the design argument, and run long here where that is warranted.** A long `//!`
is right when the module upholds an invariant in a way the code does not show, when an obvious
construction was rejected for a non-obvious reason, when a measurement drove a shape that
otherwise looks arbitrary, or when something is duplicated deliberately and a reader would
otherwise "fix" it. Absent one of those, a few lines saying what the module is for is the whole
job. Length follows from having something to say; it is not a target, and restating the code is
never warranted.

**Comments record decisions and evidence, not backlog.** There are essentially no `TODO` or
`FIXME` markers here, deliberately. Open work is an issue, where it can be found and closed.

**State each rule once, where it belongs.** `crates/tessera-engine/src/pins.rs` states its rule
under a heading saying so, then annotates each guard with which half it defends. A rule repeated
in five places is four chances to disagree with yourself.

**Prefer stable citations.** `§4`, `contracts §2.5` survive edits. `file.rs:184` does not — it
drifts on any change above the cited line, and a stale one has already stranded a worker
mid-task. `scripts/check-doc-links.py` warns on the ones that have visibly rotted.

**State load-bearing assumptions at the site.** If the assumption is load-bearing, prefer a test;
failing that an assertion; failing that, say so and say why neither was possible.

## The test

Before committing prose, ask two questions. **Could a reader who disagrees find the argument and
attack it?** If not, you asserted. **Could a reader who has never seen this system follow the
paragraph without opening another document?** If not, you wrote for yourself.
