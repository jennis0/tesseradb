# 0078 — The service takes no opinion on which variation is served

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The decision

**A variation is a general property of an artifact, not a property of labels** *(owner,
2026-08-15)*. Any artifact may carry several variations of its content; nothing about the mechanism
is specific to text.

**The service has no opinion about which one a viewer should get. The caller supplies the
ordering.** The engine resolves it and nothing else: variations are ranked by the caller, each
carries its own gate, and a viewer is served the first they satisfy — or, under
[decision 0076](0076-an-artifact-is-served-whole-or-not-at-all.md), nothing at all.

This settles the label ladder. §7.7's five tiers stop being a service mechanism and become **caller
guidance** — advice on which generating sets to produce, which is what §7.8 already uses them for.

## Why the line falls here

The distinction that decides it: **does this choose something about *access*, or about
*presentation*?**

A ladder that ranks descriptions by *quality* — this label is better than that one — is product
judgement, and it is the caller's. A ranking by *clearance* — this is how the same thing is
described to someone entitled to less — is access control, and that is the whole subject of this
service. The engine implements the second because it is the only one it can evaluate; it declines the
first because it has nothing to evaluate it with and no business trying.

**Which is why the ordering is supplied rather than derived.** The caller knows why one variation
precedes another; the service does not, and inferring it would be inventing a preference and calling
it a rule.

## What this removes

Three revisions of the model grew machinery for choosing between candidate labels — tiers, fallbacks,
selection policy. All of it goes. What remains is a ranked list the caller writes and a first-match
resolution the engine runs, over the containment test that already exists
([`annotations.md`](../design/annotations.md) §4). No variation entities, no variation identifiers,
nothing added to the deny lane.

**It also disposes of a question the model kept re-asking**: whether several descriptions of one
cluster are variations of one artifact or several artifacts. That is the caller's modelling choice
and it has always had a plain consequence — variations are one identity and the viewer gets one of
them; separate artifacts are separate identities and the viewer gets every one they satisfy. The
service implements both by implementing neither preference.

## The contradiction it resolves

The normative architecture specifies §7.7's ladder as a mechanism the service runs. This ruling
reduces it to guidance, which is a change to a normative document and is why it needed the owner
rather than the drafter. The tiers themselves are not deleted: they remain a useful account of which
generating sets a labeller should produce, cited by §7.8, and they stop being something the engine
executes.
