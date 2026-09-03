# Clients

A client asks Tessera for a view and displays what comes back. The server decides what a viewer
may see before any byte leaves it; nothing a client does can widen or narrow that. What a client
can get wrong is different in kind: showing a sample as though it were the whole set, showing a
stale view as current, showing a refusal as an empty corpus, showing a masked count as if it were
a size.

```mermaid
flowchart LR
  cred["a credential"] --> session["session plane<br/>mints a token"]
  session -- "token" --> client["client<br/>store, driver, replica"]
  client -- "token and request" --> viewer["viewer plane<br/>answers inside the mask"]
  viewer -- "counts, marks, artifacts:<br/>already masked" --> client
  client --> display["display:<br/>formats a figure, never computes one"]
```

*A client asks and displays. Every masked figure it shows was computed before it arrived.*

## What a client holds

A client holds a versioned partial copy of what has been served: a session (the token and the
identity it names), a replica of the geometry the server has answered for so far, and a presented
frame, the part of the replica currently on screen. None of this is authoritative. A replica can
be thrown away and rebuilt from nothing and the picture it produces is the same, only slower to
arrive.

The replica is kept current against a content key that changes when the corpus changes in a way
this viewer can see: new items reaching a request, or an item being denied. A client compares the
content key on its held geometry against the content key on the latest answer it has received. A
mismatch means the marks on screen may be older than the corpus. It does not mean they are wrong.
Nothing already shown is retracted; a newer answer simply exists.

| What changed | What it means for a held view |
|---|---|
| The identity a token names (a new token, a rotated key) | Every identifier the client holds is meaningless. Start over. |
| The content behind the current identity (an item added, denied, or unsuppressed) | The client's counts and marks may be older than the corpus. Mark the view stale and offer refresh. |

A third kind of change happens on disc from time to time, as segments merge and compaction moves
rows, and it never reaches a client at all: nothing a client holds is addressed by row, so none
of it is affected.

Marking a view stale is built: the store compares content keys on every answer and flips a flag a
display reads. Fetching fresh geometry on its own, without a person asking for it, is not: a
stale view stays on screen, correctly marked, until something asks the store to refresh.

## The store

The store is the part of a client that decides what to ask for and holds what comes back. It has
no idea how to draw a pixel. Drawing is a choice a host makes for itself, whether the host is a
map, a plain canvas or a table. Deciding when to ask, at what depth, and what to keep asking for
is the same question for every one of them, and getting it wrong is a mistake in data rather than
in style.

Told where the camera is, the store works out which requests are worth making and issues them.
Told a filter changed, it recomposes one expression from every active clause and sends it whole,
so a server never has to reconcile several partial ones. Given a response, it decides which
already-held geometry still answers for the view and which needs replacing, and publishes the
result as a set of values a display reads. That includes the marks to draw, the counts to show,
and the state to report.

Underneath, one object does the deciding: whether the view is moving or has come to rest, one
outstanding request together with its retries, a settle timer that asks again once the view has
been still long enough that a further answer would be worth having, and a limited look-ahead of
the area just outside what is on screen while nothing else is asked for. None of this is visible
from outside the store. A host tells it where the camera is and reads what it publishes; the
timing underneath is the store's problem to solve, not the host's to reimplement.

Annotation layers are read through a request the store makes for itself, naming the layers that
are on, rather than off cached point geometry. A cache holds geometry it has already fetched and
elides tiles it already has; an elided tile carries no artifacts, so a client reading them off the
point path would watch clusters disappear from a view that had not moved, for no reason a person
could see. Asking on its own avoids that at the cost of one extra request per settled view.

```mermaid
flowchart LR
  srv["tessera serve"] -- "framed responses" --> replica["replica<br/>what has been served,<br/>keyed by content key"]
  replica --> frame["presented frame<br/>what is on screen now"]
  frame --> render["renderer<br/>deck.gl layer, or your own"]
  gesture["pan, zoom, filter, select"] --> driver["driver<br/>a state machine:<br/>what to ask, when, at what depth"]
  driver -- "requests" --> srv
  driver --> replica
  stamp["generation stamp on a response"] -. "stale: refetch" .-> driver
```

*The store decides when to ask and what to hold. Rendering only draws what it is handed.*

## The twelve rules

The server enforces everything about what a viewer may see. It sees one request at a time and has
no idea what is on a screen, so it cannot enforce how a client presents what it already sent. The
rules below are what closes that gap: each is something a client must do on its own, and each has
a specific way of misleading a viewer if it is skipped.

| Rule | What goes wrong on screen if it is broken |
|---|---|
| Show a state that matches what happened (idle, loading, retrying, shown, empty, or refused), and let only a shown view carry a number. | A refused request drawn as an empty view looks like a place with no data, when the truth is that nothing was answered at all. |
| When a value is a sample of a set, show both how much is shown and how much there is in total, or show neither. | A bare sample count reads as the whole population. A reader takes the number on screen to be everything there is. |
| Mark a view as stale once a newer answer exists, and keep refresh one action away. | An unmarked stale view reads as current when the corpus has already moved past it. |
| Show a masked count only as what this viewer can see, never as a size or a fraction of one. | "12,040 of 12,040" beside a cluster asserts a total nobody sent. The number looks complete when it is only what one viewer happens to see. |
| Treat an absent artifact, and a filter that matches nothing, as an ordinary answer, never as a reason to explain. | Saying "no such value" or "layer not reachable" turns a filter into a way of finding out what is hidden, one guess at a time. |
| Ask for artifacts with their own request naming the layers that are on, never by reading them off cached point tiles. | As the cache warms, artifacts blink out of a view that has not changed, because a held tile stopped naming what it contains. |
| Drop held geometry, artifacts, and per-column state when the viewer's identity changes; drop held geometry when a filter changes. | Marks or clusters left from a previous login, or from before a filter was applied, stay on screen under a picture they no longer belong to. |
| Never request fewer marks per tile while zooming in than the view already has. | Points that were on screen pop out as a viewer zooms in, reading as items vanishing under them. |
| Hold an item's identifier as a full sixty-four-bit value, never as an ordinary number. | A click on a point returns the wrong record, or none at all, for an item plainly on screen. |
| Forward every header a proxy sits in front of, not only the ones it recognises. | One viewer's cached view can end up served under another viewer's token, or every request pays for a full response because nothing declares what is already held. |
| Treat a credential the server no longer holds and an expired token the same way: the session has ended. | A client that treats an outright rejection as something to retry keeps sending a request that will never succeed. |
| Choose which zoom level's tiles to request so the number of marks stays roughly what the screen can show; nothing on the wire states a depth for you. | Ignoring this either draws a handful of marks across the whole map or asks for far more than a screen can use. |

The first three keep a viewer from mistaking one kind of answer for another: a refusal for
nothing, a sample for a total, an old view for a current one. The next three keep a masked figure
honest about what it counts and what it does not explain. The rest are narrower and mechanical:
what to forget, what identifiers and headers demand, and the one request parameter the server
leaves entirely to the client's own judgement.

## Views

A deployment can serve more than one view over the same items: a different projection of the
same corpus, or a group such as a set of quarters sharing one layout. A client's store holds one
set of session-wide state and, underneath it, one replica, one presented frame, and one artifact
channel per view it has visited. Switching views moves a pointer; it never rebuilds the store.

```mermaid
flowchart TB
  shared["shared: session, mask, filters,<br/>colour, layer choice, budget"]
  subgraph store["the store"]
    cur{{"current view"}}
    A["view A: replica, frame, channel"]
    B["view B: replica, frame, channel"]
  end
  shared --> cur
  cur -. "points to one" .-> A
  cur -. "or the other" .-> B
```

*A switch moves the pointer marking which view's held state is current; both stay in the store.*

Everything that is not geometry survives a switch untouched: the session, the mask, filters,
colour and layer choice, the point budget, everything already known about a selected item. A view
that is not current has no request in flight; a view a viewer returns to is exactly as warm as
their last visit left it, bounded by one byte budget shared across every view a client has
visited, oldest-drawn evicted first.

Within a group whose views share one layout, such as a set of quarters, a switch keeps the
camera, the depth, and the selection: the same tiles at the same depth are simply asked of the
next view.
Between views that use different layouts, a switch drops the selection and refits the camera to
the new view's own extent, because a shape or a camera position in one layout means nothing in
another.

## Deployment

A deployment exposes two planes. The session plane turns a credential into a token and must never
be reached from a browser: it is gated by the deployment's own credential, which only the
integrator's own server should hold. The viewer plane is what a client actually calls with a
token, and it can be reached directly by a browser or a notebook page from an origin the
deployment has named in advance.

| Plane | Reached by | Holds | Cross-origin access |
|---|---|---|---|
| Session | The integrator's own server | The deployment credential | Never opened to a browser |
| Viewer | A browser, a notebook page, or a server on the client's behalf | A per-viewer token | An enumerated list of allowed origins, none by default |

Two shapes of mistake are worth naming because they pass every functional test and are invisible
in a screenshot. One authorises once with a single broad credential and filters per user inside
its own proxy afterwards, so every one of its users ends up seeing counts and labels computed for
the credential rather than for themselves. The other issues a token per user but mints it from
whatever claims a caller supplies, so anyone who can call the minting endpoint can claim to be any
user it names. Credential construction belongs where the authority to grant access actually is,
verified rather than merely asserted.

## Not built

Switching between two views that use different layouts refits the camera without animating an
item between its two positions; joining a viewer's held marks across such a switch, so the same
item can move smoothly from one layout's coordinates to the other's, needs a request the wire does
not yet carry.

A view that is not currently shown fetches nothing ahead of time. A switch to a view a client has
not visited fetches it at the moment of the switch, not before, and there is no separate warming
of a view a viewer has not yet chosen.

There is no way to address the server tile by tile, and no adapter that would let an engine built
around asking for one tile at a time, such as a slippy-map library, sit on top of the store. Such
an adapter would gather per-tile asks into the same region-shaped requests the store already
makes; it is specified and not built.

## Where this is tested and where it lives

The store, its driver, replica, presented frame, filter composition and artifact channel live in
`@tesseradb/client`. A drop-in map and its panels live in `@tesseradb/components` as custom
elements built on the store; a deck.gl binding lives in `@tesseradb/deck`; React hooks and element
wrappers live in `@tesseradb/react`. The Python package `tesseradb` embeds the same components
inside a notebook widget rather than reimplementing any of this in Python. An acceptance harness
drives the built components against a running deployment and checks the twelve rules through what
is actually on screen, not through a transcript of what was sent.

## Sources

`docs/design/client-interaction.md` §2–§5, §7, §9; `docs/design/client-architecture.md` §1–§4;
`docs/design/client-components.md` §1–§5, §8; `docs/design/client-obligations.md`;
`docs/design/view-switching.md` §1–§5; `clients/ts/README.md`; `clients/py/README.md`;
`clients/ts/core/src`; decisions 0095, 0096, 0097, 0098, 0099, 0100, 0101, 0102.
