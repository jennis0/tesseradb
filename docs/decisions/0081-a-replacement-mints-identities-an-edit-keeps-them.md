# 0081 — A replacement mints identities, an edit keeps them, and nothing carries across a replacement

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The premise this corrects

The design, and the review that attacked it, both took *"regeneration"* to mean **wholesale
replacement**: create a successor layer, drop the predecessor, mint 10⁷ fresh identities. Every
consequence followed from that — bookmarks expiring, edges dangling, suppressions lost, a
publish-time refusal to stop the loss, and a caller-supplied stable key made mandatory to make the
refusal expressible.

**Nothing required regeneration to work that way.** *(Owner, 2026-08-15.)* Replacement was adopted to
avoid pushing 10⁷ deletions through a deny lane sized for trickle, which is a good reason to avoid
*deletion* and not a reason to mint new identities. Once artifacts can be edited, re-running a
clustering is an **update of objects that already exist** — new membership, new content, same
identities — and **wholesale replacement becomes the rare event** it should always have been:
changing the analysis, not refreshing it.

## The decision

**Two operations, and the difference between them is identity.**

| | What it is | What survives |
|---|---|---|
| **Edit** | the caller updates existing artifacts — membership, content, gate | **everything**: identity, bookmarks, edges into them, and **suppressions**, because a suppression addresses an entity and that entity is still there |
| **Replacement** | create a successor layer, drop the predecessor | **nothing.** New identities are new objects |

**Nothing carries across a replacement, and that is correct rather than a gap.** A suppression
addresses an entity; a replacement ends that entity; the suppression ends with it. Whether the new
layer's "same" cluster is the same object is not something the service can know — the clustering may
have split, merged or reshaped it — and a service that guessed would be inventing an identity the
caller never asserted.

**So the publish-time refusal is withdrawn, and the stable key goes back to being optional.** The
refusal existed to make a suppression survive an event that ends the object it addresses; with edit
as the ordinary path, the case it protected against is a caller deliberately replacing an analysis,
where losing per-artifact state is the meaning of the operation rather than an accident. The stable
key returns to what it was before the refusal made it mandatory: a caller-side mapping across
generations, offered because only the caller knows two objects are the same.

**What replaces the refusal is a report.** An operator who suppressed something and then replaced the
layer under it should be *told* that a suppression no longer addresses anything — the same
operability signal as the fold's degraded-content report, on the same control-plane credential.
⊘ Neither is built.

## Two consequences that fall out

**The dangling-edge refusal narrows the same way.** A label layer edging into a clustering dangles
only when the clustering is *replaced*; an edited clustering keeps its identities and its edges keep
pointing at them. The refusal stays for replacement, where it is the caller declaring an intent whose
dependents they must republish, and stops being a tax on every refresh.

**Identity across regeneration stops being a weakness of the model.**
[`annotation-representation.md`](../design/annotation-representation.md) §5.2 records that a monthly
replacement makes bookmark stability *"worth much less than the argument assumes"*, which weakened
the case for retiring per-session handles. Under edit, the dominant path preserves identity and that
argument recovers.

## What is true until the edit pass lands

**Edit is deferred to its own design pass**
([`annotations.md`](../design/annotations.md) §2.3, owner 2026-08-15), so **replacement is the only
operation that exists today**. Until then a re-clustering does lose suppressions, edges and
bookmarks, and callers should be told so plainly rather than discovering it. That is a statement
about what is built, not about what the model requires — and it is the strongest argument for the
edit pass being scheduled rather than indefinite.
