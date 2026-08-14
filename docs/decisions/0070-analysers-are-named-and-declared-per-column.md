# 0070 — Analysers are named, declared per column, and there will be more than one

**Date:** 2026-08-13 · **Status:** Settled (owner ruling)
**Reads with:** [`records-and-search.md`](../design/records-and-search.md) §4.4, §7;
[`contracts.md`](../design/contracts.md) §2.2; decision 0021 (no JVM), decision 0048.
**Supersedes in part:** §4.4's "one pipeline … with no per-column configuration", as drafted r4.

## Context

§4.4 ruled the analyser as a single language-agnostic pipeline — NFKC, full case folding, UAX #29
with dictionary segmentation — with **no per-column configuration**, and named a per-column
`language` declaration as the escape hatch "if language-specific analysis is ever wanted". The
argument was that a wrong default corrupts recall silently, so there should be one default and it
should be the conservative one.

Two things sharpen that since it was written.

**The measured picture is better on coverage and worse on quality than the design assumed.** A
twenty-one script survey through the shipped analyser
([`tessera-analyse`](../../crates/tessera-analyse/), 2026-08-13) finds **no coverage hole**: every
space-separated script returns exactly its source word count, and every script written without
inter-word spaces is segmented, Khmer included. What is imperfect is *quality* in the no-space
scripts — Japanese `はとても` splits as `はと`/`て`/`も`, Thai `มาก` as `มา`/`ก` — so a query for
the mis-split word does not find the document. That is a per-script problem answered by per-script
libraries: `lindera` fixes Japanese and nothing else, and no single dependency fixes the set.

**And the need is not only linguistic.** The owner's ruling is that different analysers are wanted
*within one language* — identifiers and code split on case and punctuation boundaries that prose
must not, and prose wants folding that an identifier must not. One pipeline cannot be right for a
column of abstracts and a column of stack traces at once, and the choice belongs to the column
rather than to the deployment.

## The decision

**An analyser is a named, versioned pipeline, and a `text` column declares which one it uses.** The
family is built to hold more than one from the outset, and the corpus stops describing "the
analyser" as a singular thing.

1. **Named and versioned together.** An analyser's identity is `<name>/<version>` — the pipeline's
   name and the data-plus-shape version that determines its token stream. `unicode/icu4x-2.2/p1` is
   the first and, on the date of this ruling, the only one.
2. **Declared per column, recorded per column.** The declaration carries the analyser's *name*; the
   manifest records the full identity the build resolved it to. **Per column, not per bundle** —
   two `text` columns may differ, and a bundle-wide field could not express that.
3. **The rebuild rule is per column and unchanged in force.** Changing a column's analyser, or its
   version, invalidates that column's index and nothing else. §7's fold-merge argument — two layers
   merge only because the same versioned analyser produced them over the same values — becomes a
   per-column check rather than a global assumption.
4. **Golden vectors are per analyser.** Every analyser ships known answers per script family,
   checked against the design's rules rather than snapshotted from its own output, and the
   conformance oracle reaches each through `tessera tokenise --analyser`. This is the property that
   caught a silent recall failure on the day the first analyser landed, and it is the reason the
   next point holds.
5. **⊘ Not a plugin, and deliberately.** Analysers are built-in variants selected by name, not
   guest modules. A loaded analyser would make the token stream a deployment variable, which would
   demote the golden vectors from pinning *the* analyser to pinning only a default — and
   determinism is load-bearing for I9 and for §7's merge, exactly as it is for
   [`tessera-plugin`](../../crates/tessera-plugin/), whose own host is specified and unbuilt. If a
   plugin host ever arrives, a hosted analyser is one more named variant and this decision does not
   have to move.

## Why not defer it

Because nothing about the *format* depends on it, deferring was cheap in one sense: the index
layout does not change when the analyser does, only the token stream, and pre-release there are no
bundles to migrate ([0048](0048-no-deployments-exist-so-delete-rather-than-support.md)). What is
**not** cheap to retrofit is the per-column identity. A bundle-wide analyser field, or a manifest
that records no analyser at all, would have to be widened later while the artefacts it describes
already exist — and the failure mode of getting it wrong is the silent one: an index built by one
analyser, queried by another, matching on precisely the strings whose segmentation differs, with no
error anywhere. The field is a few bytes and the shape is the whole point of writing it now.

## What this obliges

- §4.4's "one pipeline … with no per-column configuration" is replaced by the named-analyser rule,
  and its `language`-declaration escape hatch is subsumed: a language-specific pipeline is a named
  analyser, not a parameter to the one pipeline.
- §4.4's list of absent transforms (stemming, stopwords, diacritic folding, synonyms) **stands
  unchanged for `unicode`**. It stops being a statement about analysers in general and becomes a
  statement about that one — which is what makes a future stemming analyser an addition rather than
  a contradiction.
- `contracts.md` §2.2's `declared_scalars` gains the analyser name on a `text` column, and the
  manifest gains the resolved identity per column (owed at the text declaration, #114).
- Any second analyser owes its own golden vectors before it may be declared.
