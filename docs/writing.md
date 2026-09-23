# How to write here

This governs everything in `docs/` and every code comment. Write for a competent technical reader who has not worked on Tessera: an assessor checking the security argument, an engineer evaluating the approach, a contributor arriving cold.

## Rules

1. **Describe the system, not its history.** Say what it is and why. Do not say which revision changed it, who found it in review, or what it used to be. History lives in git. Do not use phase or task numbers, "currently" or "for now".

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

The same rules. A module doc carries the design argument when the module upholds an invariant the code does not show, rejects an obvious construction for a non-obvious reason, or has a shape a measurement drove; otherwise a few lines on what the module is for. State a rule once, where it belongs, and refer to it elsewhere. No `TODO` or `FIXME`; open work is an issue.

## User documentation

The rules above apply to `docs/start/`, `docs/guides/` and `docs/reference/` as well. These pages have a different reader: someone using Tessera to get something done, who wants the next step more than the reason for it. The reason belongs in `docs/system/`, and a user page links there when a reader will want it.

A tutorial takes a newcomer from nothing to a working result along one path. A guide does one task for a reader who has finished a tutorial. A reference page is looked up, not read: every key, flag, route or method, with its type, default and what it refuses.

### Voice

Address the reader as "you" and give instructions as imperatives. "Run `tessera check`." Do not write "we", "let's", "the user should" or "you may want to".

Open a tutorial with what the reader will have at the end and what they need before starting, in two or three sentences. Then give the first step. Open a guide with the task. End a page when its last step is done, without a recap or a list of next steps.

A heading names what the reader does in that section ("Build the bundle"), or, in reference, the thing described (`[serve]`). A section holds as much as one step needs. Do not put a heading over every paragraph.

Use a numbered list for steps done in order and a bulleted list for items that are parallel. Everything else is prose. On these pages no list item starts with a bold phrase, whatever rule 5 allows elsewhere.

An admonition box is for something that loses data, exposes an item a viewer should not see, or is not built yet. An ordinary sentence goes in the text.

### Evidence

Every command on a page was run, and its output is pasted as printed. Cut long output with a line reading `...`, and never retype it. The numbers are the ones the run printed, even when they are untidy. When a page is revised, the commands are run again.

Examples use real data and real names: the GeoNames extract the tutorial builds, its `feature_class` column, the refusal the server returns. Do not use `foo`, `my_column` or `example.com` when a real value exists.

Quote an error message exactly, and say what to change.

### What gives prose away as generated

`scripts/check-register.sh` rejects the stock phrases of generated documentation on these pages as well as the vocabulary in the Register section; the script holds the list. It cannot catch the following, and review looks for them:

- Paragraphs of the same length, each closing with a sentence that restates it.
- Sentences opening "X lets you" or "With X, you can", or a colon reveal ("The result: a map").
- Signposting: "Here's how", "Now that you have", "As we saw", "It's worth noting".
- Rhetorical questions, reassurance ("don't worry") and enthusiasm.
- Three examples when one would do, or two options put side by side for balance.
- Generic claims no run could contradict ("Tessera handles large data efficiently"). Give the figure and its conditions, or cut the claim.

### Before and after

> ✗ In this tutorial, we'll walk through building your first Tessera map. By the end, you'll have a powerful, interactive visualisation of your data, ready to explore!
>
> ✓ You will build a map of every place in the GeoNames extract for Ireland and open it in a browser. You need the `tessera` binary and the extract, which the first step downloads.

> ✗ **Credentials:** Tessera uses credentials to ensure that only authorised clients can access the session plane.
>
> ✓ The session plane refuses a request without the session credential. Put the credential in a file readable only by the service's user, and name the file in `session_credential_file`.

> ✗ That's it! You've successfully deployed Tessera. Now that your service is running, you can explore filters, annotation layers and more.
>
> ✓ *(Nothing. The page ends after the last step.)*
