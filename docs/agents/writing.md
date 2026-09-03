# How to write here

This governs everything in `docs/` and every code comment. Write for a competent technical reader who has not worked on Tessera: an assessor checking the security argument, an engineer evaluating the approach, a contributor arriving cold.

## Rules

1. **Describe the system, not its history.** Say what it is and why. Do not say which revision changed it, who found it in review, or what it used to be. History lives in git and in `docs/decisions/`. Do not use phase or task numbers, "currently" or "for now".

   > ✗ "r3 replaced r1's retirement stamp, because for suppressions that was fail-open."
   >
   > ✓ "A suppression is removed only when it is lifted. A retirement stamp would let it expire and re-expose the item."

2. **Mark what is not built, at the claim.** Present tense about absent machinery reads as an assurance. Write "Not built yet:" followed by what happens instead and whether that is safe. Put it where the claim is made, not only in a preamble.

3. **Each paragraph stands alone.** Expand an acronym or project term on first use in each document. Say what a cross-reference is for ("the rejected alternative is in §7.2"), and do not use one in place of the text.

4. **Cover the substance and stop.** No introduction restating the title, no summary restating the body, no alternatives nobody proposed. If a reader cannot get a paragraph at reading speed, once, it is either padded or compressed.

5. **Emphasis is rare.** Bold only for a defined term at its definition, or at the head of a list item. Do not call a design choice novel or key. Give a mechanism the space its consequences warrant, not the space it took to get right.

6. **Draw structures, flows and state machines.** Mermaid in a fenced block with a caption. A diagram that duplicates a paragraph is padding; one that replaces it is right. A three-way comparison is a table.

7. **Say what is true, precisely.** Distinguish measured from modelled from assumed, and say which. Record negative results ("F3: not confirmed by measurement"). Say what something does not do where the scope is contestable. Argue rather than assert: a reader who disagrees should be able to find the reason.

8. **British spelling**, and the established security vocabulary: conservative label join, boolean expression indexing, partial evaluation, Non-Truman model, compartmented MAC.

## Register

Plain technical English: subject, verb, object. Short sentences, one idea each. Do not write any of the following.

- **Aphorisms or slogans as rules.** Write the rule: "The service does not refuse a deny because it is busy."
- **Antithesis.** "Not X, but Y", "X, not Y", "this is not X; it is Y". Say what it is.
- **Metaphor as jargon.** load-bearing, discharges, obligation, owes, the lane, the seam, the frontier, the gate, the fold, the spine, rides, machinery. Use the technical word. "Fail-open" and "fail-closed" describe access decisions only.
- **Abstract nouns for actions.** "when the entry is removed", not "the retirement position of the entry".
- **Em-dashes and stacked clauses.** If a clause matters, make it a sentence. If not, cut it.
- **Justifying a rule by its failure mode or importance.** "which is the point", "by construction", "deliberately", "importantly", "note that", "the failure this exists to prevent". Give the reason once, in a clause.
- **Anecdotes as justification.** "Caught in review twice", "shipped three defects". Keep the reason, drop the story.
- **Symmetry for effect.** Paired clauses, triads, mirrored sentences.
- **Absolutes and drama.** "silently", "quietly", "catastrophic", "never", "always", unless the claim is absolute.
- **Hedging.** "arguably", "in practice", "generally", "tends to". Say what is true or leave it out.
- **Pronoun-led summaries.** Name the subject.
- **Instructions disguised as observations about the reader.** Say what to do.

## Code comments

The same rules. A module doc carries the design argument when the module upholds an invariant the code does not show, rejects an obvious construction for a non-obvious reason, or has a shape a measurement drove; otherwise a few lines on what the module is for. State a rule once, where it belongs, and refer to it elsewhere. Cite `§4` or `contracts §2.5`, not `file.rs:184`. No `TODO` or `FIXME`; open work is an issue.
