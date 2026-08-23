# 0087 — A layer's edges are all within a level or all between them, and the two are used for different things

**Date:** 2026-08-18 · **Status:** Settled (owner ruling)

## What this answers

[Decision 0082](0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md) separated a layer's tree
from its levels and its summary table gave two shapes — a tree with no levels, or levels with no
lineage. [`annotation-representation.md`](../design/annotation-representation.md) §6.2 describes
**three**, the third being levels *and* containment edges, where a ward is a ward everywhere on the map —
described there as *the administrative case*, after its motivating example.

The declaration only ever had values for two, so the third could not be expressed at all. That left
an ambiguity nobody could resolve by reading: such a hierarchy's containment is naturally *cross-level* — a state at level 1 sits inside a country at level 0 — but the implementation
resolved a parent within one level, so an edge like that was refused. Neither reading was wrong
against the corpus, because the corpus had no way to say which was meant.

Two questions had to be answered together: **which direction may an edge run**, and **what is it
for**.

## The decision

**A layer declares one of two edge shapes and may not mix them.**

- **Within a level** — `kind = "nested"`. The clustering case: every artifact at level 0, lineage
  entirely in the edges.
- **Between levels** — `kind = "tiered"`, the shape that was missing. Each tier sits inside the one
  above it, and every edge runs from a **strictly coarser** level to a finer one. It need not step to the immediately next level: real
  taxonomies skip, and a city sitting directly under a country because that country has no states is
  a fact about the data rather than a hole in a ladder.

Which shape a layer has follows from its declaration and is never inferred from the edges, on §6.2's
standing rule. A layer declaring no lineage that carries an edge is **refused**, an edge running
against the levels is **refused**, and a parent key resolving in two coarser levels is **refused**
rather than settled by search order.

**And the two shapes are used for different things.**

| | Within a level | Between levels |
|---|---|---|
| What the edges are | **roll-up** — the ladder a cut climbs | **information** — what contains what |
| A coarser view is | an ancestor, chosen by the server | another level, chosen by the client |
| `artifact_budget` | trades depth for count | **inert**, as on a flat layer |
| `prune_children` | picks the frontier | nothing to prune between levels |

## Why the cut does not climb a cross-level edge

**Substituting a parent cluster for its children is an honest coarsening; substituting a state for
its counties is not.** A cluster is an abstract blob, and drawing the parent instead of the children
still says something true about that region. An administrative feature is a shape with a name: drawing
one state where the neighbouring regions are still drawn as counties produces a map that is
internally inconsistent, from a server trying to be helpful.

**A tiered layer already has a resolution control, and it is the client's.** Picking *counties*
rather than *states* is a deliberate semantic act. A budget that silently overrode it would answer a
question the client did not ask, and mixing levels in one response is the visible symptom.

So on a tiered layer a budget takes nothing, exactly as on a flat one — there is no depth to trade. An
over-large response is the **artifact ceiling's** business, which refuses rather than truncating; the
cut must never begin sampling to reach a number
([decision 0083](0083-the-frontier-is-a-request-time-budget.md)).

## What the edges are for instead

Structure, delivered to the client: which states are in a country and which cities in those states,
so a client can nest what it draws, or filter to one subtree while still drawing the wider map. That
is the rendering pipeline's need, and it is met by carrying a **parent identifier** on the artifacts
frame (contracts §3.2; the disclosure rule is Appendix C, **C29**): a parent is named only where it
is in the same response, and a parent that exists but was withheld reads as **null, identically to a
root**.

An earlier draft of this ruling proposed answering containment as a **predicate** — hand the server
two identifiers, get back one bit. It is declined. A client rendering two hundred features would
issue forty thousand calls to reconstruct a tree it should have been handed, and the structure is
part of the payload rather than an oracle to consult.

## The name

**`tiered` names the structure; `administrative` named an example.** The other values describe
shapes — `flat` has none, `nested` is a tree, `stacked` is levels piled up independently — and the
map industry's own word for this one, *admin level*, belongs to a single domain. The shape does not:
a subject taxonomy and a biological classification are the same thing, and the first layer published
against it is arXiv's category tree, where `kind = "administrative"` reads oddly.

`stacked` and `tiered` are the two levelled shapes and the difference is audible — piled up
independently, against ordered strata that relate. Administrative boundaries remain the motivating
example throughout the prose, which is what an example is for.

Two alternatives were rejected. `subdivided` is more precise and over-promises: it implies the
children exhaust the parent, which is true of a taxonomy and false of the general case this admits.
`nested_levels` is unambiguous and sits one letter from `nested`, in exactly the place a confusion
would cost most.

## What this does not change

**Nothing about how a viewer's own verdict is reached.** Every artifact is still tested on its own
masked count against its own criterion, with no input from its lineage and none from the viewport
([decision 0080](0080-the-frontier-is-a-per-artifact-test.md)). This ruling decides only which of
the artifacts a viewer may see are the ones drawn, and what structure they are told about the ones
they were given.

**Containment is still verified and reported at build time for both shapes.** A child holding a
member its parent does not is a fault in either; how much of a parent no child holds is reported for
both, because a split that loses members is what makes a parent appear without its children.

## The alternative, and why not

**Let a budget climb cross-level edges too**, making one mechanism serve both shapes. It reads as
simplicity and is not: it gives a levelled layer two controls for one job, which agree while a
hierarchy is tidy and diverge exactly when it is not — leaving a client a rule to learn about which
wins. The case it would serve is a dense region drawn at county level and a sparse one at state
level, which is **per-branch depth** and is deliberately unspecified (§7.5, ⊘): it changes what a cut
means, and the agreement property two budgets rest on is written for the single-depth form.
