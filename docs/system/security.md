# Security

Every count, cluster, density figure and label Tessera serves is computed from inside the
requesting viewer's own authorised set, not filtered into that shape afterward. This chapter
states that guarantee in full: the adversary it is built against, the five properties that make it
up, how each is enforced, what is accepted rather than closed, and what is not claimed.

## The adversary and the boundary

Three parties reach the system, and the design treats them differently.

A viewer holds a session token: whatever credentials it presented at authorisation, and as many
requests as it likes against the resulting access. This is the adversary the properties below are
built against: someone who can hold any grant, ask anything, and read every response, but cannot
forge a credential or bypass the authorisation step itself. The evidence in this chapter, and the
register in particular, states what such a viewer can learn.

A bundle holder holds the built artifact on disk: the manifest, the per-deployment key, the full
term index and the geometry. None of the properties below defend against this party. Anyone with
the bundle already has the masks, the term index and the coordinates; the identifier scheme
described below adds nothing against them, and none is claimed.

An operator drives the control plane: ingest, deletion, suppression and compaction. The design
treats this party as trusted. The write path's own correctness against a trusted operator is that
chapter's subject rather than this one's.

A client (the TypeScript or Python library, or a component built on it) is not a trust boundary at
all. Every count, sample and label it receives has already been computed inside the principal's own
mask before it left the server. What a client can get wrong is truthful display, not disclosure;
that is covered in the clients chapter.

```mermaid
flowchart LR
  subgraph untrusted["outside the boundary: the adversary"]
    viewer["a viewer<br/>valid token, any grant,<br/>unlimited requests, a clock"]
    client["client code<br/>in the viewer's hands;<br/>never decides what is visible"]
  end

  subgraph trusted["inside the boundary"]
    session["session plane<br/>turns a credential into a token<br/>that names the viewer's terms"]
    serve["tessera serve<br/>composes the viewer's set once,<br/>answers only from inside it"]
    control["control plane<br/>operator: ingest, delete,<br/>suppress, compact"]
    bundle["bundle and log on disc<br/>everything, including the<br/>identifier key"]
  end

  issuer["your identity provider"] -- "session credential" --> session
  session -- "token" --> client
  client -- "token + query" --> serve
  serve -- "counts, samples, labels:<br/>from the viewer's set only" --> client
  control --> serve
  serve <--> bundle

  holder["a bundle holder"] -. "has everything;<br/>no property below holds against them" .-> bundle
```

*What crosses each boundary. A viewer receives responses computed inside its own mask; an
operator's writes are trusted; a bundle holder already has everything the server has.*

## Every quantity is computed from the viewer's own set

A served quantity MUST be computed from inside the requesting viewer's authorised set alone. A
count, a density cell, a cluster's shape or a label taken over the whole corpus and then checked
against that set before display is a defect, not a filtered view.

An item carries a set of terms and a token satisfies a set of terms; an item is visible to a token
when the two sets intersect. The viewer's set is composed once per request, before anything reads
it, and everything that counts, draws or labels reads that one set. A filter narrows which of the
authorised items are drawn or counted and can never widen the set, so adding a filter cannot
introduce an access defect. The viewer's set is the only path to the geometry, and a build check
fails if any other path is added.

## A label is served only when its whole basis is visible

A label MUST be served only if every item it was built from is inside the requesting viewer's
authorised set. If one member is outside it, the label is withheld in full. The check runs on the
authorised set, never on a filtered one, so narrowing a query cannot make a withheld label appear,
and it runs on every request.

## Samples are taken after masking

Where more items are in view than a response carries, the sample MUST be drawn from the viewer's
own authorised set. A viewer with a narrow grant sees a sample of what they can see, never the
visible remainder of a sample taken over the whole corpus.

## A client never sees an entity id

Inside the server every item is addressed by an entity id: a dense integer assigned at ingest, in
order of the item's access terms, and the key under which its terms, memberships and labels are
stored. It is an implementation detail of the index, not a property of the data, and the index may
renumber it. An item's identity outside the server is the external id the operator supplied and the
`tessera_id` the client is given.

The entity id MUST NOT appear in anything a client can read. What it would disclose is small:
because ids are dense and ordered by access terms, a viewer holding a few could estimate a lower
bound on how many items they cannot see and how the visible ones group by access. No content is at
stake; content is protected by the first property, and no request accepts an entity id. The
`tessera_id` a client receives is a keyed permutation of the entity id, so that two of them reveal
nothing about whether their items are adjacent and a client cannot enumerate them. The permutation
is not cryptographic and, given what it hides, does not need to be.

## An incomplete answer is refused

A response that was not computed in full MUST be refused. It MUST NOT be returned as though it were
complete, and MUST NOT be returned as an empty result standing in for "not answered".

**Not built yet:** the design also states this rule for compartments, stores whose data must be
kept physically apart rather than masked: a compartment a token cannot reach contributes nothing,
and one the system cannot reach is an error rather than an empty contribution. A deployment has one
compartment today, so neither case can arise and neither rule has anything to test it.

## What this does not claim

| Not claimed | Why not |
|---|---|
| Cryptographic strength of the client-facing identifier | Not needed. The permutation hides a lower bound on the number of hidden items and their grouping by access, a low-severity channel. An adversary who recovered the key would be back at that channel and nothing more. |
| A defence against a bundle holder | Anyone holding the bundle has the key, the term index and the coordinates. None of the properties above are claimed against them. |
| Isolation of compartmented partitions | **Not built yet.** The design specifies physical separation for data that must be held apart. A deployment today has one store, so no isolation beyond masking is available. |
| Agreement between the two authorisation functions | See below. |
| Closure of the timing channel | Accepted and unquantified; see the register. |
| Protection of data at rest | This chapter covers what a viewer can learn from responses. Data on disc has a different adversary and is covered where the storage is described. |
| The client as a trust boundary | The client never decides what is visible; every value it holds has already passed the server's mask. Its rules concern truthful display and are in the clients chapter. |
| Availability and denial of service | Outside this chapter. The serving chapter covers admission and load shedding. |

The caller supplies two functions: one derives terms from an item's access label, the other from a
viewer's credentials. Every property above rests on the two agreeing about what a term means, and
nothing checks that they do. The only plugin that exists passes strings through unchanged, so its
two functions cannot disagree; the check cannot exist until a plugin does whose functions could.

## Residual disclosure

Every quantity above is stated as a rule with no exception. The channels below are the accepted
exceptions: things a viewer can learn beyond a single item's own visibility, each judged worth
carrying rather than worth closing. The specification enumerates thirty-three such channels
individually; the table groups ones that share a shape and states each group's full severity range
and status. The rightmost column names every specification row a group absorbs, so a reader working
from the specification or the conformance suite can trace a code to its place here; that column is
the only place one of those codes appears in this chapter.

| Channel | What a viewer can infer | Severity | Status and why | Specification rows |
|---|---|---|---|---|
| Density and coarse counts restate an already-served quantity | That a viewer's own visible items cluster together in a region, and how many visible marks a tile holds, at a finer grain than a bare count | Low | Accepted, no new channel: the exact masked count is already served for any region or zoom level; these are coarser or cached views of the same figure | C1, C18 |
| Response time and corpus activity track work outside the viewer's own set | How much data outside their own set a request walked, from timing; and that the corpus is being written to, from a staleness signal, in both cases without any content reaching them | Low | Open for the core timing question: unmitigated and unquantified. Accepted elsewhere in the family, on the ground that a principal already knows how much of the corpus is its own | C4, C14, C15, C19, C21, C24, C25, C26, C31 |
| A stable identifier admits existence-probing and linkage | Whether a held identifier still resolves, which timestamps a delete, a suppression or a grant change; that two principals or two sessions are looking at the same item; a caller's own external identifiers can leak structure if the caller chooses to keep them | Medium | Accepted as the intended trade of a bookmarkable identifier. Probing how identifiers moved across a key rotation is closed specifically: no parameter exists to vary | C6, C17, C20 |
| Caller declarations the service cannot verify | Content or a vocabulary value the caller asserted rather than derived from membership, served exactly as declared | Medium, high if mis-declared | Accepted, the caller's control: provenance is not something the service can check. All five are built: a vocabulary's visibility, a layer's label and its membership requirement are declared in the corpus file, and supplied content arrives through the control plane with its declaration or is refused. The specification's rows still carry stale not-built markers for three of them | C7, C12, C23, C27, C28 |
| A vocabulary's values, and a densely pinned ordinal | A value's existence, gated the same way a label is; where an author pins codes densely, the largest visible code coarsely bounds how many values exist | Low to medium | Closed for existence, gated on a visible member carrying the value. Accepted for the ordinal, an owner ruling that set-size leakage from a caller's own chosen numbering is not a threat this system defends against | C11, C22 |
| Node metadata and a routing registry, once compartments exist | That a grouping or a label draws on a given compartment | Low to medium | Node metadata is closed. The registry is accepted, and meaningful only once compartmented partitions exist. **Not built yet:** with one store today there is nothing to separate | C13, C16 |
| A drill-down names relations already served in full | An artifact's served parents; an item's own satisfied labels, reachable views and scoped values | Low | Accepted, bounded to what the requesting principal already sees in full | C29, C30 |
| The highlight and browse verbs answer a second question over an existing candidate set | Nothing beyond what two ordinary filtered requests would already disclose | Low | Accepted. Built; the specification's row still carries a stale not-built marker | C32, C33 |
| Extractive-tier background frequencies | Corpus-wide term distributions, if drawn from the live corpus | Low | Closed: a fixed public reference corpus is used instead of the live one | C5 |
| Pre-intersection filter cardinality | A raw match count taken over unauthorised records | High if exposed | Closed structurally: no route serves a match count before it has been intersected with the viewer's own set | C8 |
| Text relevance scores and ranks | Corpus-wide statistics that would let unreadable content be inferred | High if ranking were added | Closed by scope: filtering is boolean only, and no ranking exists | C9 |
| Vector similarity results and thresholds | Neighbours that vary observably with items outside the viewer's own set | High if post-filtered | Closed structurally: threshold filters push the viewer's own set down before any comparison runs | C10 |
| Checked and found not to leak | Nothing; recorded because each was checked | None | A node's bounding box and hull shape are recomputed from masked members only; label existence omits every unsatisfied candidate from the response | C2, C3 |

## Evidence

| Property | How it is checked | What is not covered |
|---|---|---|
| Every quantity computed from the viewer's own set | Compared, value for value, against an independent second implementation across three planted states, over every served surface: tiles, the points batch and the density layer. The same property was probed manually against a running server across the request contract and the memory-safety surface beneath it, and no route past it was found | Served artifacts travel on their own frame and are not part of this comparison today |
| A label served only when its whole basis is visible | Compared against an independent implementation with two principals differing by exactly one item inside a label's basis, and the difference asserted before anything downstream rests on it | None |
| Samples taken after masking | Compared against an independent implementation with a wrong-shaped stand-in, a sample taken in storage order rather than from the authorised set, that the comparison is required to disagree with | None |
| A client never sees an entity id | Scanned across every wire surface, the sub-cell stream and the logs for a byte pattern matching the underlying identity, with a planted true positive on every scan confirming the scan itself works | None |
| An incomplete answer is refused | The built half is covered by tests around shared in-progress work and cancellation, though not by the differential test form the design describes | The two compartment rules have no test at all, for the reason stated above: nothing exists yet for either to apply to |

## Sources

`docs/design/architecture.md` §4, §6, Appendix C; `docs/design/conformance.md` §0, §4.6;
`docs/design/client-obligations.md`; decisions 0014, 0017, 0019, 0023, 0024, 0027, 0061, 0101;
`docs/evidence/memos/2026-08-14-access-control-redteam.md`.
