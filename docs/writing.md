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

The rules above were written for the system chapters, whose reader is assessing a design. The pages in `docs/start/`, `docs/guides/` and `docs/reference/` have a different reader: someone learning Tessera or using it to get something done. Where this section and the rules above differ, this section wins on those pages. British spelling and the evidence rules apply everywhere.

There are three kinds of user page, and each is written differently. A tutorial teaches a newcomer by walking them through building something. A guide helps someone who already knows the basics do one task. A reference page is looked up, not read.

### Tutorials

A tutorial reads like a knowledgeable, patient person sitting beside the reader while they do something for the first time.

It opens with a promise. Say who the tutorial is for, what the reader will have built by the end, what they will understand that they did not before, roughly how long it takes, and what they need. Then show the journey before walking it: a short outline of the stages, so every later step has somewhere to hang and the reader always knows where they are.

It teaches a mental model, not a list of commands. Before each step, say what we are about to do and why it is needed; after it, say what happened and what to notice in the output. By the end the reader should be able to explain the pipeline to someone else: source data, a declaration that describes it, a build that turns both into a bundle, a server with three separate doors, and tokens that decide what each viewer sees.

Introduce each idea at the moment it is needed, one at a time, with the concrete case first and the name second, in plain words. "Every place on this map is tagged with who may see it. Tessera calls that tag an access label." A term the reader has not met is never used without this. Deeper explanation can wait for a link to the system chapters; the basic "what is this" cannot.

Give the reader visible progress early and often. A clean `check`, a count that matches what was expected, a map appearing: point them out.

Never let the reader get lost. Every step works exactly as written, and the expected output is shown so they can tell they are on track. Where something is likely to go wrong, say what that looks like and what to do. Follow one path; choices belong in guides.

Invite a little exploration where it teaches something: remove a required key and watch the check fail, or apply a second filter and watch the count change.

End by consolidating: what the reader built, the ideas they now understand, and one or two natural next steps.

The voice is warm, confident and conversational. "We" is natural for work done together and "you" for what the reader does. Admit what looks odd and explain it. Personality comes from specifics and from having a view, not from enthusiasm or exclamation marks.

### Guides

A guide opens with the task and when you would need it, in a few sentences. Each step says briefly why it matters, then how. Terms are defined at first use or linked to where they are. Warmth is welcome, but the reader has come to finish a job, so a guide explains less than a tutorial and moves faster.

### Reference

A reference page is terse and complete: every key, flag, route or method, with its type, default, what it does and what it refuses. One or two sentences per item. No narrative.

### Evidence

Every command on a user page was run, and its output is pasted as printed. Cut long output with a line reading `...`, and never retype it. The numbers are the ones the run printed, even when they are untidy. When a page is revised, the commands are run again.

Examples use real data and real names: the GeoNames extract the tutorial builds, its `feature_class` column, the refusal the server returns. Do not use `foo`, `my_column` or `example.com` when a real value exists.

Quote an error message exactly, and say what to change.

### What makes prose read as generated

The tell is emptiness, not structure. An introduction, an outline and a closing summary are all right in a tutorial. What gives them away is being generic: a promise of "a powerful, interactive visualisation", an outline that repeats the headings, a summary the reader could have guessed before starting. Fill each one with specifics that only this tutorial could contain.

Most of the other tells are in the sentences.

- **Compound sentences, one after another.** Two clauses joined by ", and", ", so", ", but", a semicolon or a colon, sentence after sentence. Join two clauses when they make one thought, not to avoid a full stop.
- **Runs of short sentences in the same shape.** "It keeps shapes true. It stretches areas. It has no default." Four sentences in a row that begin "You'll". Chopped prose reads as mechanical as chained prose. Write as a person talks: mostly plain sentences of varied length, with a longer one where an idea needs carrying through and a short one where it lands.
- **Symmetry.** Paragraphs of the same length, lists whose items all take the same shape, parallel clauses balanced against each other. Embrace asymmetry. Mix sentence lengths and styles: a statement, then a question the reader would ask, then an aside. Let one paragraph run long and the next be a single line. A page with some unevenness reads as written by someone.
- **A colon followed by a list of three.** "One program does all the work: it checks, builds and serves." Say one thing, or show the three as they happen.
- **Sentences about the text instead of the subject.** "There are six stages, and each one leaves something you can look at." "This is the step that decides what a viewer sees." Cut them, or replace them with the fact they gesture at.
- **Framing that nobody would say aloud.** "The pieces every deployment is made of." "The whole path from a text file to a viewer's map." Name the actual things.
- **Metaphor for a technical part.** Doors, lanes, gates. Use the real name and explain it in plain words.
- **Claims no run could contradict.** "Tessera handles large data efficiently." Give the figure and its conditions, or cut the claim.
- **Enthusiasm, hype and reassurance in place of information.** "That's it!", "don't worry", "seamlessly".
- **Explanations that restate the command** instead of saying what it does and why.
- **Three examples where one would do,** or options set side by side for balance.

`scripts/check-register.sh` rejects the stock phrases of generated documentation on user pages; the script holds the list. It cannot see sentence shape, so review has to.

### Before and after

> ✗ In this tutorial, we'll walk through building your first Tessera map. By the end, you'll have a powerful, interactive visualisation of your data, ready to explore!
>
> ✓ This tutorial is for someone who has never used Tessera. We'll take the 29,935 places GeoNames lists for Ireland and put them on a map in your browser. You'll be able to search them by name and filter them by population. The commands take about four minutes to run. Half of that is compiling Tessera.

> ✗ `point_visibility` names no field to read an access label from, so every place gets the default label, `public`.
>
> ✓ Tessera decides who may see each place by giving it an access label. Each viewer holds a set of labels and sees only the places that carry one of them. A label usually comes from a column in your data. We have no such column, so every place gets the default label, `public`. Every viewer holds `public`. Everyone will see every place.

> ✗ Run `tessera build`.
>
> ✓ Now build the bundle. The build reads every row of `points.parquet` and works out where each place sits on the map. It stores neighbouring places next to each other on disc. The result goes into the `bundle` directory. From now on the server reads only the bundle, never your Parquet file.

> ✗ One program, `tessera`, does all the server-side work: it checks a declaration, builds a bundle and serves it. There are six stages, and each one leaves something you can look at.
>
> ✓ *(Cut both. The outline that follows shows what each stage does.)*
