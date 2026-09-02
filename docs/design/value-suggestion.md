# Value suggestion — typeahead over a category vocabulary

**Status:** **Normative (r4, 2026-09-02).** The six questions in §10 are ruled (owner, 2026-09-02)
and written to decisions 0118–0123; decision 0124 narrows D — the suggestion route may follow the
viewer's own cardinality (§6.3, §8).
**Built (2026-09-02, branch `design/value-suggestion`).** The fold, the boolean membership probe,
the suggestion index, `Engine::suggest`, the served verb with its two ceilings and one-in-flight
admission, the client's typeahead and the conformance differential are all in the tree
(`conformance.md` §4.6 is where coverage stands). ⊘ Still unbuilt: §6.2's bucket table **(b′)** and
§6.3's per-session set, both until the track that adds them lands; a `suggest_word_starts` opt-out;
cross-column suggestion (decision 0123); and the vocabulary read side at 10⁷ (issue #130).
**Its §6 figures are measured** at 10⁶ and 10⁷ values over 10⁸ entities — the walk and the build on
the shipped route by the implementation's bench, the representation and single-probe figures by
[`probes/2026-09-02-value-suggestion/`](../../probes/2026-09-02-value-suggestion/README.md).

**Reads with:** [`per-point-attributes.md`](per-point-attributes.md) §3.3, §3.8;
[`contracts.md`](contracts.md) §3.2 (`GET /v1/categories/{column}`);
[`filter-index.md`](filter-index.md) §1.1, §2.3; [`records-and-search.md`](records-and-search.md)
§4.3; `architecture.md` §4 (I2, I3, I12), §8.2, §8.3, Appendix C (C8, C11, C22, C23, C24, C25,
C31); decisions
[0039](../decisions/0039-multi-valued-categoricals-are-slow-path-only.md),
[0063](../decisions/0063-category-postings-serve-public-listings-and-never-per-viewer-ones.md),
[0067](../decisions/0067-term-timing-is-accepted-for-text-and-keyword-postings.md),
[0069](../decisions/0069-filter-do-not-rank-sharpens-to-no-corpus-global-statistics.md),
[0090](../decisions/0090-a-vocabulary-has-one-visibility-axis.md),
[0093](../decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md);
[`probes/2026-08-07-category-membership/`](../../probes/2026-08-07-category-membership/results.md),
[`probes/2026-08-03-dict-fst/`](../../probes/2026-08-03-dict-fst/README.md).

---

## 1. Summary

A viewer typing into a category filter should be offered the values that complete what they have
typed — and **only values they could see anyway**. This document designs that surface for
vocabularies of hundreds of thousands to millions of values.

Five positions, each argued below:

- **The gate is `/v1/categories`' gate, unchanged.** A `public` vocabulary is suggested as
  authored; a `derived` one suggests a value iff the viewer can see an item carrying it, evaluated
  against the composed mask (`M_auth`), never against the request's filters. Suggestion is C11's
  channel through a third door, and the door opens onto the same predicate (§3).
- **It is a new verb, `GET /v1/categories/{column}/suggest`, not a third form of the enumeration.**
  Typeahead never pages, orders by what matched rather than by key, matches on titles as well as
  keys, and wants match spans for highlighting. Bolting `?q=` onto a key-ordered cursor endpoint
  gets every one of those wrong (§5).
- **Matching is a fixed, declared rule: case-folded, Unicode-normalised prefix over the key, the
  title, and every word start of either.** No fuzziness, no ranking — ordering is the matched
  text's own, and every rule is a function of the schema rather than of the corpus or the caller
  (§4, §7). **A count beside each suggestion is served on request**, as C8's `and_cardinality`
  against the composed mask, and never orders anything (§3).
- **Per-keystroke work is one boolean posting probe per value walked, and its timing channel is
  accepted.** A suggestion index per vocabulary gives the prefix a contiguous entry range; each
  value in it is tested against the composed candidate, until the page is full or a walk budget of
  10⁵ is spent — **measured on the shipped route at 10⁷ values: 61–68 ms median, 81–84 ms p99 for
  the sparsest viewer on a quiet host, inside the owner's 10–100 ms; the index is ~1 GB of mapped
  file at that scale, its entry dictionary 91–124 MB of it** (§6). The probe is the question `/v1/categories` asks, but not by the route it
  asks it: a boolean `intersects` on the mapped posting is a deliverable of this design and both
  doors move onto it (§6.2). The walk's cost tracks the values under the prefix, hidden ones
  included, which is a timing read on what `derived` withholds; the owner accepted that channel on
  2026-09-02, and §8 registers it as **C31**. A per-session visible set **closes** it and is the
  second route: built on demand where the viewer's own composed cardinality is small enough to
  sweep, with the probe route answering the first keystrokes on a session-column pair and every
  wider viewer (§6.3, decision 0124).
- **Keywords stay refused.** The corpus refuses autocomplete on string columns twice, normatively,
  and this design does not reopen it: the shape that wants suggestion is an **open, `derived`
  category**, which exists. What it needs at a million values is this surface and the scaling
  items §9 names, not a new family (§2).

---

## 2. Scope: categories, and why not keywords

**A category has a value set; a keyword does not.** `filter-index.md` §1.1 refuses "prefix
autocomplete" over a string column because offering suggestions "would manufacture a value set for
a type that has none", and `records-and-search.md` §4.3 restates it word for word for `keyword`: no
value set, no listing, no autocomplete, the dictionary never served. Those refusals are of a
*surface*, taken with the owner's correction that **strings are not categories**
(`filter-surface.md` Appendix R, 2026-08-08).

Nothing structural prevents a keyword suggestion under membership-derivation — a per-layer sorted
dictionary gives a prefix an ordinal range, and an ordinal scan under the candidate gives the
visible ordinals. Three things make it the wrong design regardless:

- **The cardinality is the corpus's.** A keyword vocabulary is open-ended and per layer: a
  `submitter` column is 542,489 distinct values in 2.4M items (measured, records §4.3); an `id`
  or `doi` column is one per item. The size argument is what decides it: a per-session visible set
  over 10⁹ ordinals is 125 MB per session per layer, against the **1.25 MB** a 10⁷-value category's
  set saturates at (measured, §6.3) — two orders of magnitude, per layer, for a value set nobody
  authored. And a suggestion over `doi` is meaningless anyway.
- **The identity is layer-scoped.** Suggestions would have to be unioned across every layer's
  dictionary and deduplicated by string per request, with no stable identifier to hand back: a
  keyword ordinal never crosses the trust boundary, so the client would receive a string it
  re-submits as a `prefix` or `eq` operand. That works, and it is a category with the codes taken
  out.
- **The need is already served by a declaration.** A column whose values are worth suggesting has
  a value set the operator cares about, and `[[vocabulary]] value_set = "open", visibility =
  "derived", width = "u32"` declares exactly that: keys minted at ingest, a code per row, derived
  postings per value, gated listing. What such a column lacks today is not a surface but scale
  (§9).

So the recommendation is (§10 A): **categories only; a keyword-shaped need becomes an open
`derived` category.** The one need this does not answer is **tags** — several values per item —
which are multi-valued and remain specified-and-unbuilt behind decision 0039's fence
(per-point-attributes §3.7). A multi-valued category, when built, gets this surface for free: the
gate and the index are per value, and the row shape does not enter.

**Text** is out of scope for the same reason keyword is, and more so: a token index is an analysed
artefact, and a suggestion over it is a surface over data-derived tokens nobody authored.

---

## 3. The gate

Everything `/v1/categories` says about who may be told a value name holds here, and the reasons
are the same reasons. Stated once so the review can attack it in one place:

**`public`.** The value set is an authored assertion that its names disclose nothing
(per-point-attributes §3.8). Suggested as authored to any principal with a session. No predicate.

**`derived`.** A value is suggested iff at least one of its members is visible:
`members(v) ∩ candidate ≠ ∅`, where `candidate` is the composed verdict — the session's fragment
less the overlay's denials, plus buffered entities the overlay admits — exactly
`filter::candidate`'s set and `Engine::categories`' input. Derived per request from inside
`M_auth` (**I2**), never maintained (§3.3's non-monotonicity argument stands), and self-retiring
under suppression.

**Against `M_auth`, never against the filtered mask.** A viewer with an active filter is still
offered every value they may see, not only the values that survive their filter. This is **I3**'s
rule for labels and **I12**'s direction applied to an offered enumeration: filters may narrow what
is drawn, never widen what is disclosed — and a suggestion set narrowed by the filter *would*
disclose less, so it is admissible as a later extension through the filter contract (§8.2), but it
is a different surface — the candidate becomes the filtered set, which is per request by
definition — and is not this one.

**Counts, on request** (owner ruling E, 2026-09-02). With `?counts=true` each suggested value
carries `count`: `|members(v) ∩ candidate|`, C8's `and_cardinality` against the composed mask,
computed per request and never precomputed — the shape Appendix C's C8 row already names for a
legend with counts, so it is not a new register row. It is exact, it is the viewer's own number,
and it is computed only for the values the page serves, so the work is bounded by `limit`. The
extents half is counted in the same sweep that finds post-build membership (§6.2), per code rather
than as a set. Without the flag no number is served, and a client redrawing on every keystroke
should ask for counts only where it will show them.

**No ranking by the corpus, and not by the count either.** Ordering by frequency or popularity is
decision 0069's corpus-global statistic — a rank that moves with items the viewer cannot see. The
served count is the viewer's own and would be admissible as a sort key under I2, but a
count-ordered page is a top-*k* over the prefix, which §8.2 forbids for the reason it forbids
top-*k* filters: the result would depend on which values were examined before the budget ran out.
Ordering is lexical over the matched text (§7), a function of the schema alone; the count is
information beside a row, never the row's position.

**An unresolvable or invisible value is indistinguishable from an absent one in outcome.** No
status, no field and no gap in the page separates the three, which is §3.8's requirement, and §6 is
the construction that obtains it. **Not in work**: §3.8's indistinguishable-*in-work* property is a
*filter's*, obtained there because a `derived` column's operand is answered by the masked scan and
never by the postings (C24). Both listing surfaces read the postings per value walked, so the time
a request takes is a function of how many values sit under the prefix, hidden ones included. That
is the channel `architecture.md` Appendix C registers as **C31**, argued in §8.

**The refusal rule carries over.** Where a `derived` column's member sets cannot be read the
request is `500 fail-closed` naming the column, as `/v1/categories` is: an empty suggestion list is
a real answer, and serving it for an underivable predicate makes the two indistinguishable.

**A read that fails part-way through the walk refuses the whole column too**, rather than serving
the values found so far. Refusing on the value the read failed at would make the refusal a function
of the prefix the caller typed — an oracle over value names in a fault state. This is a narrow
case: `PostingsReader::open` validates every record when the column is opened (§9), so a read that
fails afterwards is a host IO fault rather than a shape of the data, and no request-shaped input
reaches it. §8 names what residue that leaves.

---

## 4. What matches

**The rule is declared, fixed, and the same for every principal and every value.** It is applied
to the query and to every indexed string alike, so a match is an equality of folded bytes and
never a judgement.

**Folding.** Unicode NFKC, then default case folding, then whitespace collapsed to one space and
trimmed.

The first two are the `unicode` analyser's normalisation (records §4.4, decision 0070), and they
are the same rule rather than a second one — one fold in the codebase, one thing for the contract
to say. `tessera-analyse` exposes no fold today: it normalises inside tokenisation and returns
tokens, so this design factors NFKC-plus-full-case-fold out of `Analyser` as a public function and
calls it, rather than duplicating it. ⊘ That factoring is not built, and neither is anything else
here.

The whitespace collapse and the word-boundary rule below are **this surface's own**, not the
analyser's: they decide what entries the index holds and nothing about how a `text` column is
tokenised. A consequence worth naming, because it is invisible to an English-language reader: a
script written without spaces yields no word boundaries and therefore **no word-start entries**, so
such a vocabulary is suggested on whole-key and whole-title prefixes alone.

**Three entry kinds per value**, each an `(folded string, value)` pair in one sorted index:

| Entry | String indexed | Example, value `key = "cs.LG"`, `title = "Machine Learning"` |
|---|---|---|
| key | the whole folded key | `cs.lg` |
| title | the whole folded title, where one exists | `machine learning` |
| word start | the folded title (or key, where no title) from each word boundary after the first | `learning` |

A word boundary is a transition into a letter or digit from anything else, after folding. Keys of
the shape `machine_learning` therefore also yield `learning`. A value with no title indexes its key
as the title would be.

**A match is a prefix of an entry.** `q = "mach"` matches the title entry; `q = "lear"` matches
the word-start entry; `q = "cs."` matches the key entry. Every match is a contiguous range of the
sorted index, found by two binary searches.

**The span a client highlights is derived at response time, not stored.** The index records where
an entry starts as a **character offset into the served string** — the key or the title as an
author wrote it. To emit `match`, the request re-folds the served string forward from that offset
until `q`'s folded bytes are consumed; the characters consumed are `len`. That is O(|q|) per emitted
value, needs no second copy of the folded text beside the entry, and is what lets `match` be
reported in characters of the string the client is about to draw rather than of a folded form the
client never sees (§5.1).

**An empty `q` matches everything**, and returns the first `limit` visible values in index order.
This is the picker's initial list — the values a viewer sees before typing. It is the widest range
there is, so it is also the request most likely to spend the walk budget on a sparse viewer, and
`more` is what it says so (§5.1).

**What is deliberately not matched.** Infix (`"chine"`), fuzzy or edit-distance matches, stemming,
and synonyms. Each is a judgement rather than a rule; each widens the surface for a reviewer to
argue over; and the word-start entry covers the case that infix is usually wanted for. Adding one
later is an index-shape change and nothing else, because §6's visibility construction does not
depend on how an entry was derived.

---

## 5. The API

### 5.1 The verb

```
GET /v1/categories/{column}/suggest?q=<text>&limit=<n>[&counts=true][&view=<group>:<key>]
```

Response, `200`:

```json
{
  "column": "primary_category",
  "q": "mach",
  "values": [
    { "code": 41207, "key": "cs.LG", "title": "Machine Learning",
      "match": { "field": "title", "start": 0, "len": 4 }, "count": 18342 },
    { "code": 9,     "key": "stat.ML", "title": "Machine Learning (Statistics)",
      "match": { "field": "title", "start": 0, "len": 4 }, "count": 2210 }
  ],
  "more": true
}
```

- **`column`** is the caller's spelling, echoed — as `/v1/categories` does.
- **`q`** is echoed as received (not folded); the client matches it to the request it has in
  flight. Bounded at 256 bytes; over it, `422`.
- **`values`** carries at most `limit` values, each once, in §7's order. `code`, `key` and `title`
  are `/v1/categories`' fields with the same meaning; `title` is `null` where no author wrote one, as the enumeration serves it.
- **`match`** says which field matched and where, in **characters of the served string**, so a
  client can highlight without re-implementing the fold. `field` is `key` or `title`; a word-start
  match reports the field the word came from and the start of that word.
- **`count`** is present iff `counts=true`: the number of items carrying the value that this
  viewer may see, exact, computed per request (§3). Absent otherwise — never `0` or `null` as a
  stand-in.
- **`more`** is `true` iff the walk stopped before its range was exhausted — the page filled, or
  the walk budget was spent (§6.2). Either way the client's response is the same: type more. On a
  spent budget it is a **thresholded, pre-mask count of the values under the prefix**, on the wire:
  it says at least `max_suggestion_walk` values sit there, hidden ones included. That is the
  quantity §8 registers as C31, at one bit of resolution, and it is registered as on the wire and
  not only in time. The alternative — `more: false` on a spent budget, so the flag counts visible
  values alone — was declined: it under-reports, and a broad prefix would hide visible values
  behind a flag saying there were none. **The spent-budget form is the probe route's.** On §6.3's
  per-session set the flag is exact — that route walks only visible positions, so it never spends a
  budget and sets `more` only when the page filled — and `more` is the one field on which the two
  routes may differ. Which route answered is not on the wire.
- **`limit`** defaults to and is clamped by `selection.max_suggestions` (§5.3), a deployment
  constant published on `/v1/meta`. **`limit=0` is `422`** on its own reason: a zero-length
  suggestion page is a request for no answer. The enumeration refuses it because a zero-length page
  with a cursor that cannot advance is an infinite loop; this verb has no cursor, so it needs the
  simpler reason.

**Address resolution is `/v1/categories`'.** An entity-scoped column is its own name; a
group-scoped family is view-addressed through `?view=` or the `{column}@{key}` pin, resolved at
the same site, through the same gate, with the same `404` for a view the principal may not reach
and the same `422` for a bare scoped name (contracts §3.2, r59). `404 unknown` covers "no such
column", "not a category" and "vocabulary missing" identically.

**Admission: off the compute queue, one in flight per session.** The walk is **not** behind the
compute-admission gate — a per-keystroke surface queued behind viewport renders would be unusable —
and it does not run on the reactor either: it faults on mapped files and probes up to
`max_suggestion_walk` postings, which is not work to do on a thread that must stay responsive. It
runs in `spawn_blocking`, with **at most one suggest in flight per session**. A request arriving
while one is in flight for that session is answered `429` with `retry_after_s` before any work is
done, and the client retries once the in-flight one returns; a keystroke debounce makes it rare,
and the bound is what stops a client that does not debounce from turning a held key into a queue.
Nothing here is per-request accounting: it is one flag on the session.

The enumeration's own justification is corrected in the same change (contracts §3.2): "a bounded
walk of an in-memory map with no mask composition, no file IO" is true of the `public` arm only. A
`derived` column composes the candidate and probes a memory-mapped posting per value it walks,
which is exactly the work this verb does over a narrower range.

### 5.2 Why not `?q=` on the enumeration

The enumeration (`GET /v1/categories/{column}`) is a legend's endpoint: it resolves the codes a
client drew, or pages the whole set ascending by key with a key cursor. A `q` parameter on it was
the obvious first draft, and it fails on four counts:

- **Order.** The enumeration orders by key because a key is a total order and therefore a cursor.
  A typeahead orders by *what matched* — a title match on `machine learning` must sort where the
  user expects `m`, not where `stat.ML` falls in key order.
- **Paging.** Typeahead never pages: the user types another character. A cursor over matches
  invites a client to walk the whole match set, which is the enumeration by another name.
- **Titles.** The enumeration's cursor is the key; a title match has no place in that order.
- **Composition.** `codes`, `after` and `q` together have no sensible meaning, and the contract
  would spend its words on which combinations are `422`.

The two verbs share one engine gate — the same `visible(code)` predicate, the same candidate
composition, the same refusal — which is what "two doors, one gate" requires. They do not share a
shape. **Recommendation (§10 B): the new verb.**

**And they differ on counts, which is not an inconsistency.** The enumeration keeps refusing them
(contracts §3.2): its `?codes=` form is a legend's resolve, and a legend carrying a count per drawn
code is the per-viewport breakdown surface §8.2 owns, arriving through the filter contract or not
at all. A suggestion page is a prefix the caller typed and at most `limit` values, so a count sits
beside a row the caller asked for rather than beside every code on the screen. Both numbers would
be the same C8 `and_cardinality`; what differs is whether the caller chose the rows.

### 5.3 `/v1/meta`

`selection` gains `max_suggestions` — the suggestion page's ceiling and default (recommended
default 20). Published on the same argument as `max_category_values`: a client must be able to tell
a short list that means "that is all" from one that means "the deployment truncated", and `more`
alone does not distinguish a deployment whose ceiling it hit. A performance knob, so it defaults
(SA §7); what a principal may be *told* is `visibility`'s question, settled before the page is cut.

`selection` also gains **`max_suggestion_walk`** — §6.2's walk budget, recommended default 10⁵ —
on `max_tiles_per_request`'s argument: a client that receives `more: true` on a page it did not
fill should be able to read it as the deployment's budget rather than as its own arithmetic being
wrong. A performance knob, so it defaults, and a deployment constant identical for every principal.

`selection` gains a third field with §6.3's second route: **`max_suggest_set_entities`** (⊘ name
provisional; recommended default 10⁷), the composed cardinality at or under which a per-session
visible-value set is built for that viewer instead of probing per value. It is published on the
same argument as the other two — a deployment constant, identical for every principal, and a client
that cannot see it cannot tell an exact `more` from a budgeted one. What it discloses to the caller
is which side of it their own cardinality falls on (§8, decision 0124).

**No capability flag.** A `suggest: true` on each `category` block would be a constant — every
category column has the surface — and there is no server older than this revision for a client to
read it defensively against (decision 0048). `api_version` stays at **1** and `bundle_format` does
**not** move: the verb and the two `selection` fields are additive on the wire, and nothing in the
bundle changes.

### 5.4 Cross-column suggestion

A single search box over every category — `GET /v1/suggest?q=` — is composable from the per-column
verb and is **deferred** (§10 F). Each column is gated on its own member sets (§3.2's per-column
predicate), so a server-side form saves round trips and nothing else; it earns its place when a
client wants it, and its response shape is the per-column one with `column` per value.

---

## 6. Performance: the construction

The target is **~10⁷ values** (owner, 2026-09-02), keystroke cadence, and thousands of concurrent
sessions. The owner's trade-off, stated the same day: **memory as low as possible; 10–100 ms per
keystroke is acceptable** where residency and speed pull against each other. **Every figure below is measured** unless marked
otherwise, by two rigs: the walk, the build, the extents sweep and the counts by the
implementation's own bench (`tessera-bench`'s `suggest_walk`, over the shipped
`suggest::walk` and `ColumnPostings::intersects`), and the representation and single-probe figures
by [`probes/2026-09-02-value-suggestion/`](../../probes/2026-09-02-value-suggestion/README.md):
10,132,181 distinct folded GeoNames names as the vocabulary, 10⁷ values over 10⁸ entities with Zipf
and uniform membership for the postings, the shipped `SortedDictWriter`, `PostingsSpool` and
`ColumnPostings` readers, one thread, page cache warm. Device-cold and concurrent figures are **not
measured**. One structure per vocabulary, no state per session on the probe route, and a
per-session set built on demand for a viewer narrow enough to sweep (§6.3).

### 6.1 The suggestion index — per vocabulary, principal-blind

One sorted index of §4's entries over **every** value of the vocabulary, visible to anybody or not.
It names no principal, so one copy serves every session (decision 0093's cadence argument).

**Shape.** `SortedDict` carries no payload and refuses duplicate keys, and two values can fold to
one entry string (`cs.LG` and `CS.lg`, or two titles differing only in case), so the index is six
mapped files rather than one:

| File | What it holds |
|---|---|
| `entries.dict` | the **distinct** folded entry strings, sorted — a `SortedDictWriter` file, turned into a range `[lo, hi)` by two binary searches |
| `runs.bin` | one `u32` per entry string plus a sentinel, indexing `payloads.bin`; entry `e`'s run is `runs[e]..runs[e + 1]` |
| `payloads.bin` | **12 bytes** per (entry string, value) pair, ordered within a run by (kind, key): the value's **dense position**, the character offset of the word start into the served string, and the entry kind and field (`key` or `title`) as flags |
| `codes.bin` | one `u32` per dense position: that value's code |
| `strings.bin` + `offsets.bin` | the served key and title per value, in position order, with `u32` offsets |

The **dense position** — the value's rank in the vocabulary's key order, `0..V` — rather than the
code, because codes are scattered at random over the declared width (§3.4); it is also what the
per-session set in §6.3 is over. The run structure is what makes the
duplicate case ordinary rather than an error: a prefix range is a range of entry strings, and the
values under it are the concatenation of their runs. The last two files exist so a served value's
`key` and `title` come out of the index instead of out of a walk of the minter's map, which is §9's
residency question.

**Where the files live — the engine's cache directory, not the bundle.** r2 said "the bundle's
runtime directory"; no such directory exists, and `contracts.md` §2.1 fixes what a bundle contains.
The index sits beside the fragment cache under the engine's local cache directory, which is this
repository's one precedent for a derived, undigested, rebuildable file the engine writes for
itself: the manifest does not name the index, no digest covers it, and it is rebuilt from the
vocabulary at every open. The files carry no format version and need none — `Engine::open` clears
the whole tree before it builds, and each build writes a fresh numbered subdirectory, so a stale or
foreign index is unreachable rather than served.

**Representation — measured, and the repository's own dictionary format wins.** At 10⁷ values,
key-only entries, one process per cell, median of three builds after a shared sort:

| Structure | Resident | Build | Prefix range, median |
|---|---|---|---|
| `BTreeMap<String, u32>` (the minter's shape today) | **705 MB heap** (70 B/key) | 4.7 s | 0.2–1.1 µs |
| Sorted string arena + `u32` offsets | 328 MB heap as built, 229 MB tight | 1.3 s | 0.2–1.1 µs |
| FST (`fst` 0.4) | **95.0 MB file** (9.5 B/key) | 8.6 s | 0.4–1.2 µs |
| Front-coded block dictionary (`SortedDictWriter`, restart 16) | **90.8 MB file** (9.1 B/key) | **2.4 s** | 1.2–2.7 µs, fail-closed checks included |

With word-start entries (2.20 entries per value on GeoNames — names average 1.64 words; descriptive
titles will pay more) the dictionary is 124 MB of file, the FST 132 + 134 MB, the arena 671 MB of
heap and the `BTreeMap` 1.6 GB with a 2.46 GB peak.

**The residency claim is the whole index, side arrays included — and r2 priced the payload wrong.**
The table's file sizes are the entry dictionary alone. r2's "~90–200 MB of side arrays" costed a
payload record at 4 B; the record as built is **12 B**, and the served strings carry 8 B of offsets
per value on top. The honest figure is dict + 12 B × entries + 12 B × values + the served strings'
own bytes. **Measured on the bench's fixture**, which derives 4 entries per value: **105 MB at 10⁶
values** — 48 MB of payloads, ≈ 34 MB of strings, 12 MB of offsets, 4 MB of codes, 4 MB of runs and
≈ 7 MB of dictionary — and **1.06 GB at 10⁷** on the same shape. At GeoNames' 2.2 entries per value
the payloads are ≈ 264 MB at 10⁷ rather than 480. So the index at 10⁷ is of the same order as the
1.6 GB of heap the minter holds today, not a quarter of it; what it keeps is that the bytes are a
mapped file the kernel may evict rather than anonymous heap. On the
adversarial arm (32-character random hex, nothing shared) the FST stops winning as the dict-fst probe
found: 319 MB and 29.6 s against the dictionary's 297 MB and 3.5 s.

**The dictionary format is the recommendation**: a mapped file the kernel may evict rather than
anonymous heap, within 5% of the FST's bytes, 3.6× faster to build, and no new dependency — its
writer and reader exist for keywords. The prefix lookup is 1–4 µs for every structure and is
irrelevant to keystroke latency. The arena's residency is defensible at 10⁶ (35–72 MB) and not at
10⁷ under "memory as low as possible".

**The vocabulary's own store is the larger residency question.** The `BTreeMap` row above *is*
`VocabularyMinter::codes` at 10⁷ values — 705 MB of heap for the key half alone, before titles and
the `assigned` set — so at this scale the suggestion index is not an addition beside the vocabulary
but the shape the vocabulary's read side should take: a mapped dictionary over keys with a `u32`
code array beside it, and the mutable map kept only for an `open` vocabulary's mints since the last
generation. §9 carries it.

**Cadence.** The index is built **at open**, on the pool, into the engine's cache directory, from
the vocabulary as `VocabularyMinter` holds it (manifest values and every `SEGMENTS-<n>.json`
extension). ⊘ It is not a digested build artefact; making it one is the vocabulary read-side item
§9 carries, and until then a cold start pays the build. **The sort was the build cost at 10⁷ and no
longer is.** r2 measured 4.7–6.7 s to derive and sort 10⁷ key-only entries and **34–38 s** for 22M
key-plus-word-start entries single-threaded, and called a parallel sort the implementation's first
fix-it-now item; that is what shipped — a rayon sort over an arena. Measured end to end, entries
derived and sorted and all six files written: **1.0 s at 10⁶ values and 10.7 s at 10⁷**.

**It is then held behind an `Arc` and cloned across publications.** A flush changes which entities
carry a value and changes nothing this index holds, so rebuilding per generation would pay the sort
for no change. Changed values or titles are the *necessary* condition rather than the whole rule:
the rebuild is dispatched on the pool when the side map exceeds **4,096 values**
(`SUGGEST_REBUILD_SIDE_VALUES`), because one mint per ingest batch against a 10⁷-value vocabulary
would otherwise dispatch a 10-second sort per batch and every result but the last would be
superseded before it landed. Waiting is free — a value in the side map is suggested exactly as one
in the base is — so the rebuild is owed to *residency*, and a threshold is the shape residency
wants. The rebuild keeps, by sequence number, whatever arrived while it sorted, and the superseded
directory is unlinked after the swap, which a live mapping survives.

Between rebuilds the index is kept complete by a small mutable side map — a `BTreeMap` over the
same folded entries, ranged the same way and merged into the walk at query time (§6.2 step 3). It
is fed **at the mint site**, so a value minted by an ingest is suggestible on the next keystroke
rather than at the next rebuild. A **title amendment** (per-point-attributes §6) puts the new
entries in the side map, puts the value's old title and word-start entries in a retraction set the
walk consults, and asks for a rebuild. A lone amendment therefore never triggers one — the
threshold above governs — and its retraction persists until a rebuild comes, which is correct
because the side map is complete: the walk serves the new title and skips the old whether or not
the base has caught up. So **the base index is stale only under amendment**, and only in the
direction of holding a title an author has replaced, which the retraction set covers until the
rebuild lands. Values are never removed from a vocabulary (codes are pinned forever,
§3.4), so it is otherwise never stale, only incomplete.

**`counts=true` composes a candidate on a `public` column too.** A count is the viewer's own
`and_cardinality` and needs the mask whatever the visibility says; it only ever narrows a number,
and never widens the set of values served, which the visibility alone still decides. A column whose
vocabulary has no postings therefore **refuses** a counted request rather than serving the page
with the numbers left out: a page whose `count` fields were silently absent reads as asked and
answered.

**Word-start entries multiply the index by the average word count** — 2.2× on place names, more on
descriptive titles. It is a linear cost with a
knob (`suggest_word_starts = false` on the vocabulary, ⊘ if wanted) and not a design problem.

### 6.2 The request — the enumeration's probe over a prefix range

Per keystroke:

1. Fold `q`; binary-search the index for `[lo, hi)`; open the side map's range the same way.
2. Compose the candidate once — `filter::candidate`, as `Engine::categories` does — and build the
   extents sweep (`category_membership`), the codes carried by candidate entities in every
   post-build extent. Both are per request today and stay so; they are bounded by the session's
   fragment and by post-build ingest respectively, and neither depends on the prefix.
3. Walk the two ranges in merged folded-string order, skipping anything the retraction set holds
   (§6.1). For each entry: skip a value already emitted; otherwise test `carries(code)` — the
   extents set first, then `members(code) ∩ candidate ≠ ∅` against the memory-mapped posting,
   **as a boolean that short-circuits, never as a materialised intersection**. Emit a visible value
   with its match span (§4). Stop when `limit` values are emitted or the **walk budget** is spent.
4. `more` is `true` iff the walk stopped before the range was exhausted — the page filled, or the
   budget ran out (§5.1).

For a `public` column step 3 has no predicate; every entry in the range is emitted in order.

**The boolean predicate is a deliverable of this design, not a reuse of an existing one.**
`ColumnPostings` answers membership today through `entities(code)` → `resolve_union`, which
materialises the value's corpus-wide posting and intersects it afterwards; there is no boolean
route to short-circuit on, and `carries` is written in terms of the materialising one. This design
adds **`ColumnPostings::intersects(value, &candidate) -> io::Result<bool>`**, short-circuiting over
the sources at the first container the two share, and moves `carries` onto it — so the two doors
keep one predicate (§5.2) and the enumeration, which runs it once per value in the whole
vocabulary, gets faster for the same change.

The size of what it closes is measured on the probe's own two routes at 10⁷ values over 10⁸
entities, on a *visible* head value under a scattered candidate: **0.1–0.6 ms** median and 7.9 ms
worst through the materialising `narrow`, against **0.15–16.6 µs** median and 74 µs at p99 through
`Bitmap::intersect`. Those are the probe's routes, not the shipped one; §6's flatness bound is
measured on them, and the bench below measures `intersects` on the shipped route.

**The walk budget** is the number of values examined per request — a deployment constant,
`selection.max_suggestion_walk` (**default 10⁵**, measured below), published on `/v1/meta` on the
same argument as every other ceiling there. It bounds **latency**, not disclosure: the channel's
bound is the vocabulary itself, which the enumeration walks whole and unbudgeted, and a budget only
ever narrows what one request examines. A viewer who sees none of the values under a broad prefix
examines `max_suggestion_walk` values and is told to type more.

**Cost of a single probe, measured at 10⁷ values over 10⁸ entities.** A probe against a hidden
value is **0.06–0.13 µs** at the median whatever its member count — 3.4 µs at p99, 103 µs at the
very worst — because a mapped view intersected with a disjoint candidate touches only the
containers whose keys coincide. A visible value is **0.15–16.6 µs** at the median through the
boolean route, p99 to 74 µs.

**Cost of the walk, measured on the shipped route** — `suggest::walk` over the built index and
`ColumnPostings::intersects`, the same function the verb calls, one thread, page cache warm, quiet
host, budget 10⁵, a one-character prefix, the gate supplied as a closure rather than composed from
a session:

| Viewer | 10⁶ values | 10⁷ values |
|---|---|---|
| 0.01%, contiguous | 13.0 ms median, 21.6 p99 | **61.2 ms median, 81.2 p99** |
| 0.01%, scattered | 16.4 ms median, 23.7 p99 | **68.2 ms median, 83.6 p99** |
| 1% | — | 1.17 ms median, 2.22 p99 |
| 10%, scattered | — | 0.08 ms median, 0.67 p99 |

**r2's 1.9–10.4 ms was `Bitmap::intersect` alone, with the record already in hand.** The shipped
walk first has to *find* the record, by a binary search over the keyed base's code array — codes are
scattered over the `u32` width (§3.4), so there is no arithmetic route from a code to its record.
**Split into the three things one probe does** — measured over the codes a budgeted walk actually
probes, at 10⁷ values, in nanoseconds, the median of the sparsest viewer's probes:

| Stage | What it does | Cost |
|---|---|---|
| `search` | binary search over the keyed base's 10⁷-entry code array | **550** |
| `view` | `read_posting`: the record slice, and `BitmapView::deserialize` where the record is Roaring | 190 |
| `test` | `hits`, the existential intersect | 20–70 |

for a whole call of 730–811 ns, of which the **search is 68–72%**. An earlier reading of the same
search as 13–29% of a probe (299 ns of 1,037) is not contradicted: it timed searches back to back
with the code array warm in cache, and interleaving the view and the test — which is what the walk
does — evicts the array between them. The walk *around* the probe is not where the time is: the
fold, the two binary searches over the index, the payload and code reads per entry and the emitted
set together cost **9.1–9.5 ns per value examined**, about 1.5% of a keystroke. So essentially all
of a keystroke is the probe, and two thirds of the probe is finding the record.

A build-time position → record-ordinal array is **not** the fix: a code with no members has
no record, so rank in code order is not the record ordinal, and a code → record map built at open
would be 40–80 MB resident per column, which the memory-first ruling declines. The sparsest
viewer's keystroke is still inside the owner's 10–100 ms at the median *and* at p99.

**(b′) A bucket table over the code's top 20 bits takes most of the search back, for 4.2 MB.** The
search is slow because two dozen comparisons over a 40 MB sorted `u32` array miss cache on the last
several of them. A table of 2²⁰ `u32` offsets beside the code array — **4.2 MB per column**, free to
build because the array is already sorted, and needing no map from a code to a record — leaves ~10
records per bucket, one or two cache lines, so the search becomes one or two misses rather than
eight: ~150–250 ns, and the sparsest viewer's spent budget at 10⁷ falls from 62–82 ms to
**~28–40 ms** *(modelled from the stage table above, not measured)*. ⊘ **Not built** until the track
that adds it lands; every walk pays the full search meanwhile.

**A per-record container-key sidecar was priced and declined.** It would answer *can this record
possibly meet the candidate?* from the record's container keys without deserialising the posting
body — attacking the `view` and `test` stages, 26–32% of a probe, and not the search's 68–72%.
Reading the sidecar run is itself a random touch, so where it does reject it saves ~90–140 ns:
12–17%, or 62 ms → ~53–55 ms *(modelled)*. Against a **scattered** candidate it rejects nothing at
all — a 0.01% scattered viewer's mask touches every one of the entity space's 1,526 containers, so
every record's keys meet it, the view and the test are paid anyway and the sidecar's own touch is
pure loss for the slower of the two viewers. At 10⁷ records it is 82 MB per column (measured over
the fixture's 20.8M container keys): half of a code → record map's bytes for a quarter of its
saving, on the contiguous shape alone.

**~30 ms is the probe route's floor.** No per-request route pays less than finding the record and
constructing its view, whatever it saves on either. A keystroke materially under that is reachable
only by not probing per request at all, which is what §6.3's per-session set does.

The figures are post-fix: the bench found two defects in the implementation before it could measure
it — an allocating `sources()` per probe, and a read of a value's served strings before the gate —
and both were fixed before these numbers were taken. **⊘ Concurrency is still not measured**, and
the host is part of the number: with two other suites running, the same code measured 84 ms at the
median and **424 ms at p99**. The quiet figure is the one to design against, and the loaded one is
the reason the concurrent measurement is owed.

Viewers at 1% and 10% fill the page inside a 10³ budget. The 10⁴ budget r1 first proposed **never
fills the sparsest viewer's page** on a one- or two-character prefix — 0 of 100 pages filled, six
values found — so the default is 10⁵: every measured page fills, and a viewer who sees nothing
under a broad prefix pays the full budget, which is the 61–68 ms measured above rather than the
6–30 ms r2 modelled from the intersection alone. Raising the budget costs nothing when the page
fills early, since the walk stops.

**Counts add one `and_cardinality` per served value**, on the mapped view without materialising the
intersection. Measured for a page of twenty: **+1–2 ms** over the walk at the 1% and 10% viewers,
and lost in the noise at 0.01%, where the page never fills. r2 modelled ≤ 12 ms from the
materialising `narrow` probe; the boolean-route measurement is well inside it.

**The extents sweep is not small, and it is measured.** Step 2's `category_membership` pass over a
post-build extent — the codes candidate entities carry, which both listing verbs pay — is **49.7 ms
median, 74.6 p99 over a 10⁶-row extent** for a 10%-contiguous viewer, 1.7 ms for a scattered one. It
scales with the extent, not with the prefix or the budget, so at a 10⁷-row extent it would exceed
the keystroke budget on its own. §9 carries it. **⊘ Composing the candidate is still not measured**:
it needs a built 10⁸-entity bundle and an authorised principal, which is a campaign rather than a
bench, and `probes/2026-08-07-category-membership/` is the nearest figure.

**One modelled figure did not reproduce, and the register row must not carry it.** C24's 1.26–2.1 ms
for a hidden value with many members, measured over 2.4M items, never appeared here at any sparsity
or shape: the hidden probe stayed under 0.13 µs at the median. The expensive probe at this scale is
the *visible* head value under a scattered candidate through the materialising route, which the
boolean route removes. Whether the C24 fixture's shape exists at 10⁸ is **not established** either
way; the walk's constant is the one measured on this fixture.

### 6.3 The second route: a per-session visible-value set, built on demand

⊘ **Specified, not implemented.** Every request takes §6.2's probe route today. What follows is the
second route and the rule that picks between them, and none of it is in the tree.

Where a viewer's own composed candidate is small enough to sweep, the walk stops probing per value
and reads a set instead: a per-`(session, resolved column, generation, overlay_version)` Roaring
bitmap over the **dense value positions** (V bits — the index range maps entries to values, so the
bitmap is over values and not over entries), computed by one pass over the column's `u32` value
column under the candidate. A keystroke then iterates the set bits inside `[lo, hi)`
(`reset_at_or_after`, already in the dependency) and touches nothing hidden.

**It is built on demand, and no keystroke waits for it.** The first suggest on a
`(session, resolved column)` pair dispatches the build on the pool and is itself answered by the
probe route, as is every keystroke until the build lands; from then on that session's keystrokes on
that column walk the set. Nothing is materialised per session in advance, which is decision 0093's
rule, and nothing blocks on a 46–61 ms pass.

**Both routes answer the same page.** They evaluate the same predicate against the same composed
candidate, so the values, their order and their match spans are identical, and which route answered
is not on the wire. The one field on which they may differ is **`more`**: the probe route may set it
on a spent budget where the set route, walking only visible positions, answers exactly. That
difference is the C31 bit already accepted (§8) — a thresholded pre-mask count of the values under
the prefix — and not a new quantity.

Four rules govern the set:

1. **A key that no longer matches the live generation and overlay version is discarded, never
   served.** Serving it would offer a value whose last visible member has been suppressed since,
   which is fail-open. The key carries both for that reason.
2. **One build in flight per key.** The per-session admission of §5.1 already serialises a session's
   keystrokes, so this needs no second mechanism.
3. **The route follows the viewer's own cardinality** ([decision 0124](../decisions/0124-the-suggestion-route-may-follow-the-viewers-cardinality.md)).
   The set is built only where the composed candidate's cardinality is at or under
   **`selection.max_suggest_set_entities`** (⊘ name provisional; recommended default 10⁷, the
   46–61 ms build point measured below), a deployment constant published on `/v1/meta`. A wider
   viewer stays on the probe route, where the walk fills a page in under a millisecond anyway — the
   1% and 10% rows of §6.2 — so the ceiling costs that viewer nothing. This is the one place in the
   system where a route is a function of how much the principal can see; §8 argues why it is
   admissible, and 0124 records the narrowing.
4. **A column with no entity-space value column never takes the set.** A blob-resident `derived`
   category has postings but nothing to sweep, and the all-postings alternative is **0.3–6.8 s at
   every sparsity** — it opens every record whatever the candidate — so it is not a token-cadence
   route at 10⁷. Such a column stays on the probe route, or it declares `index = true`.

Measured at 10⁷ values over 10⁸ entities:

- **The build is the value-column pass**: 0.2 ms for a 10⁴-entity viewer and 46–61 ms for a
  10⁷-entity one through a plain bitset (the Roaring `add`-per-entity form is 6× slower). Linear
  extrapolation to a 10⁹-entity full-mask candidate is ~6 s *(modelled)*, which is rule 3's reason
  for a ceiling rather than a slower build.
- **The set is small**: 13 KB of Roaring for a viewer seeing 0.06% of values, saturating at 1.25 MB
  (= V bits) once a fifth are visible. Ten thousand concurrent sessions is at most 12.5 GB as
  bitsets and far less as Roaring for sparse viewers. **Residency and eviction are decision 0093's
  byte budget**, unchanged.
- **Per keystroke it is 0.3–2 µs** at the median; a very sparse set's p99 rises to 60–85 µs where the
  iterator crosses many empty containers, and a plain bitset scan is ≤ 1.3 µs at p99 everywhere.

Its cost is that an entry goes **cold on every flush and every deny**, since rule 1 puts
`overlay_version` in the key so that a suppression retires a value fail-closed. A fold-epoch key for
the base half — which would keep a set across the writes that cannot change it — is the refinement
to reach for second.

## 7. Ordering and duplicates

**Ascending by folded entry string; ties by entry kind (key, then title, then word start); then by
key.** A total order over the index, fixed at build, the same for every principal. A whole-title
match on `machine learning` therefore precedes the word-start entry `machine learning` derived from
`statistical machine learning`, which is what a reader expects and falls out of the sort rather
than from a rule.

**A value appears once**, at its first matching entry in that order. Its `match` reports that
entry.

**Nothing about the order is a function of the corpus** — no frequency, no recency, no viewer.
Decision 0069 forbids the alternatives, and the fixed order is also what makes the response
reproducible by a conforming second implementation from the vocabulary alone.

---

## 8. Disclosure

**The surface is C11's.** Offering a value name is the channel C11 registers; `/v1/categories`
publishes it under a gate and this verb publishes it under the same gate. The gate is
`visible(code)` and nothing else, so **every value served on a `derived` column with no authored
gate label has a visible member**. The qualification is C23's: a **declared** vocabulary may carry
an authored gate label per value, which *replaces* membership-derivation for it, so such a value
can be served to a principal who can see none of its members. That is the reverse of
membership-derivation by deliberate assertion and is the mechanism's point, not a leak of this
surface; C23 registers it, and ⊘ it is not built — a `gate` column in a vocabulary file is refused
at parse.

**The timing channel, accepted.** For a `derived` column the request's work is a probe per value
walked, and a value the viewer cannot see costs **0.06–0.13 µs** at the median whether it has one
member or ten million (measured, §6.2) — so what the timing carries is the *number* of values under
the prefix, at ~0.06–0.3 µs each, and not their sizes. So the response time — and
`more` on a spent budget — is a function of how many values, hidden ones included, sit under the
prefix the caller typed, and of nothing else about them. That is a corpus-wide, pre-mask
fact about a value set `derived` exists to withhold, at a resolution the enumeration's whole-set
walk never offered. **The owner accepted it, 2026-09-02**, on the reasoning that took decision
0067 for term postings: comparable systems carry the identical channel ambient and unregistered,
and here it is registered, bounded and conscious.

It is registered as **C31**, a row of its own beside C24 and C25 — C24's control is that the
postings never answer a `derived` column, and this is the first `derived` surface to carry the
channel. What bounds it:

- The quantity is **a coarse count of values under a prefix the caller typed** — never a value's
  name, never its member count (the hidden probe is flat in it, measured), never membership or
  which items, and nothing about another principal's `M_auth`. The names stay behind the gate; only
  their count leaks, and only in time, at sub-microsecond resolution per value against a network
  round trip.
- The **route** is a function of the declaration, of the request's prefix, and — on this surface
  alone — of **the viewer's own composed cardinality against a deployment constant**: §6.3's
  per-session set is built where that cardinality is at or under
  `selection.max_suggest_set_entities`, and every wider viewer keeps the probe route. §8.2 forbids a
  statistics-driven route because it makes execution time a function of how much the principal can
  see; what is consulted here is **the principal's own** cardinality, which `/v1/viewport` at zoom 0
  already returns exactly as `visible`, against a constant identical for every principal. No corpus
  statistic enters it, and nothing about another principal's `M_auth` does. Decision 0124 records the
  narrowing and what it would cost if wrong: a viewer learns which side of a published constant their
  own cardinality falls on, and nothing about anyone else.
  What *is* a function of the principal's own visible set is where the walk **stops**: it terminates
  early on the first `limit` values they can see. That is a fact about their own mask, which the
  answer's own length already gives them.
- **The budget bounds one request's work, not the channel.** A range wider than
  `max_suggestion_walk` (10⁵) reads as the budget whatever its size — a measured 61–68 ms at 10⁷
  values on a quiet host (§6.2) — but that is
  not a limit on what is carried: the enumeration walks the whole vocabulary unbudgeted and carries
  the same fact more slowly. What bounds the channel is the vocabulary.
- **It reaches the wire once, as one bit.** `more` on a spent budget says at least
  `max_suggestion_walk` values sit under the prefix, hidden ones included (§5.1) — the same
  quantity at the coarsest resolution there is, and a walk that spends its budget has already taken
  the budget's time. The register row names it as on the wire and not only in time.
- **The route that closes it is designed and priced** (§6.3): a walk over a per-session set of
  visible positions touches nothing hidden, so its time carries no count of hidden values at all.
  The acceptance is therefore bounded to the probe route rather than structural.

**The disposition is accepted, and the row is closed where the set is warm.** For an indexed column
whose viewer is inside `selection.max_suggest_set_entities`, C31 is closed once the set has been
built. It stays open on the probe route: the first keystrokes on a session-column pair before the
build lands, a viewer wider than the constant, a blob-resident column that can never take the set,
and the enumeration in every case. ⊘ The set is not built (§6.3), so today the channel is open
everywhere.

**The enumeration carries a weaker form of the same channel** — `/v1/categories` on a `derived`
column walks the whole set and probes every value, so its time is a coarse read of the vocabulary's
size — and it had never been registered. **C31 covers both surfaces.** What the suggestion verb
adds is resolution: the caller chooses the range, where the whole-set walk offered one number.

**C22 is unchanged**: codes are served as they are on the enumeration. **C8 is exercised, not
extended**: the served count is the row's own `and_cardinality` against the composed mask, per
request, never precomputed, and never a sort key. Its cost is a function of a *visible* value's
posting shape under the viewer's own candidate, which is a fact about items the viewer can see. **The index range's size under a prefix is never on the wire as a number**; `more` is one bit of
it, on the terms above.

**A fault-conditional residue, named rather than defended.** ⊘ A host IO fault on one value's
posting refuses the whole request (§3), so a caller who could induce a read failure on a record of
their choosing could distinguish that record from one the walk merely skipped. Nothing
request-shaped reaches it — `PostingsReader::open` validates every record when the column is opened
(§9), so this is a fault state of the host rather than a shape of the data, and the alternative
(refusing at the value the read failed at) would make the refusal a function of the caller's own
prefix, which is worse.

**What this does not defend against, stated so it is not oversold.** A viewer who can see one
member of a value learns the value's name; that is membership-derivation's definition. A viewer
enumerating a `derived` vocabulary by typing every prefix learns the visible set faster than by
paging, and no more of it — and, by timing, roughly how many values they were not shown.

## 9. What a million-value vocabulary costs elsewhere, on this feature's path

The suggestion surface is not the only thing that has to scale for the target Joe named, and four
of the others are on any implementation's path. None is this design's mechanism. Two are
**fix-it-now** items in the change that builds it; the second is a store format question and is
**issue #130**; the last is on the request path and is ⊘ not designed here.

- **`/v1/categories?codes=` walks every binding per request** (`Engine::categories`, the `Codes`
  arm walks `bindings()` and tests `codes.contains`). At 10⁶ values a legend resolve is a
  million-step walk per viewport — tens of milliseconds where it is microseconds today. It wants
  the reverse map (code → key) the vocabulary already has the information for. The paged form's
  `after` skip is the same walk from the start of the map, where a `BTreeMap::range` is the fix.
- **The vocabulary is persisted as JSON and held as a heap `BTreeMap`**: `MANIFEST.vocabularies`
  carries every value, each flush's `SEGMENTS-<n>.json` carries its extension, and the minter holds
  keys in a map measured at **705 MB of heap for 10⁷ keys** (§6.1). At 10⁷ the manifest is hundreds
  of megabytes parsed at open and the map is the largest resident structure in the process. The
  suggestion index's mapped dictionary is the shape the read side should take — keys in a
  `SortedDictWriter` file with a `u32` code array beside it, digested like the postings, the mutable
  map kept only for mints since the last generation — and the JSON becomes a build input rather
  than the served store. ⊘ Not designed here, and not on this surface's critical path: the
  suggestion index builds at open into the engine's cache directory and works without it (§6.1).
  It is **issue #130**, with decision 0048's licence to change the format outright.
- **`PostingsReader::open` validates every record**, round-tripping each Roaring payload
  (per-point-attributes §3.5). Measured at 10⁷ records: **1.4–1.6 s** mapped where 169k records are
  Roaring (~8.5 µs per Roaring record round-tripped), 50 ms where every record is a small array —
  proportional to the Roaring record count, not to V. Tolerable at open; a number the lifecycle
  design should carry.
- **The extents sweep is per request, and scales with the extent rather than the prefix.**
  `FilterColumns::category_membership` walks each post-build extent's value column to find which
  codes candidate entities carry — **49.7 ms median, 74.6 p99 over a 10⁶-row extent** for a
  10%-contiguous viewer (§6.2). Both listing verbs pay it, and a 10⁷-row extent would spend the
  keystroke budget on this pass alone. What takes it off the request path is a per-column reverse
  index over an extent's values, or counting the codes at flush. ⊘ Not designed here.

The **authorise arm** per-point-attributes §8 owes — vocabulary-filter cost against vocabulary
size, at cold and cached fingerprints, against principal sparsity — is also this design's
measurement: §6.2's setup *is* that evaluation, and running the arm at 10⁵ and 10⁶ values settles
the two ⊘ figures in §6.2 and the representation choice in §6.1 at once.

---

## 10. Rulings — all taken (owner, 2026-09-02)

**A. Scope — (a).** Categories only; a keyword-shaped need is declared as an open `derived` category
(§2). The refusals in `filter-index.md` §1.1 and `records-and-search.md` §4.3 stand.

**B. API shape — (a).** A new verb, `GET /v1/categories/{column}/suggest`, uncursored, ordered by
match (§5.1).

**C. Word-start matching — (a).** Key, title and word-start entries in r1 (§4).

**D. Visibility construction — (b).** Per-request posting probes with a walk budget of 10⁵, the
timing channel accepted and registered (§6.2, §8). The per-session set was the priced lever;
decision 0124 makes it the second route, taken where the viewer's own cardinality is at or under
`selection.max_suggest_set_entities` (§6.3).

**E. Counts — (b), against the draft's recommendation.** `?counts=true` serves C8's
`and_cardinality` per suggested value (§3, §5.1). The count never orders the page.

**F. Cross-column suggestion — (a).** Deferred; composable from the per-column verb (§5.4).

Each is written to `docs/decisions/`: **A → 0118, B → 0119, C → 0120, D → 0121, E → 0122,
F → 0123.** Three bind other documents — A confirms two refusals in `filter-index.md` and
`records-and-search.md` without amending either, D widens decision 0063 and narrows
per-point-attributes §3.8's indistinguishable-in-work claim, and E puts a C8 quantity on a new
surface.

## Appendix R — review trail

**r1 (2026-09-02) — drafted**, from a brainstorm with the owner: autocomplete over categories,
gated per viewer, at a target the owner then set at ~10⁷ values with memory first and 10–100 ms per
keystroke acceptable. A per-session visible-value set and a per-request probe walk were both drawn;
**the owner accepted the probe walk's timing channel the same day**, which made the walk the
construction and the set a priced lever.
The probe campaign ran the same day and chose the dictionary format over the FST and the arena,
moved the walk budget from 10⁴ to 10⁵, replaced C24's modelled hidden-value constant with a
measured sub-microsecond one, found the sort to be the build cost at 10⁷, and ruled the
all-postings route out for the lever. All six questions in §10 were then ruled — the draft's
recommendation on each except **E**, where counts are served on request.

**r2 (2026-09-02) — reviewed and promoted.** Three lenses attacked the draft; all three would have
promoted it, and none found a fail-open path. What bit: `more` was defined twice and the two
definitions carried different quantities; §3 claimed the enumeration's indistinguishable-**in-work**
property, which contradicts decision 0063 and is a filter's property rather than a listing's; the
boolean predicate the whole walk rests on **does not exist** in `ColumnPostings`, so §6.2 was
reusing something it had to build; a per-value read failure mid-walk would have refused
prefix-dependently; the index was specified against a `SortedDict` that carries no payload and
refuses duplicates; and it was to be rebuilt once per flush, which is a 38-second sort for a change
it does not see. What changed: `more` is the walk's own stopping condition and is registered as a
thresholded pre-mask count **on the wire**; §3 claims outcome alone and points at §8; `intersects`
is a named deliverable and both doors move onto it; the index is five mapped structures carried
across publications behind an `Arc` and rebuilt only when values or titles change; admission is
`spawn_blocking` with one suggest in flight per session; and §8's route bullet splits the
declaration-fixed route from the early termination on the caller's own mask. The channel is
registered as **C31**, and the vocabulary's own read side left as issue #130.

**r4 (2026-09-02) — the probe's time is split, and the set becomes the second route.** Arm 4 of the
probe campaign put **68–72% of a probe in the binary search for the record**, against a 13–29%
reading taken with the code array warm, and priced two fixes against the stages: a 4.2 MB bucket
table over the code's top 20 bits, **taken** (§6.2, ⊘ not built, ~28–40 ms modelled for the sparsest
viewer), and a per-record container-key sidecar, **declined** — 12–17% at best, nothing at all
against a scattered candidate, and 82 MB per column.
**§6.3 stops being a lever held in reserve and becomes the second route**, built on demand and keyed
by session, resolved column, generation and overlay version, with the probe route answering the
first keystrokes on a pair and the two routes differing only on `more`.
**The route follows the viewer's own cardinality** (owner ruling; decision 0124): the set is built
only at or under `selection.max_suggest_set_entities`, which narrows §8.2's rule on this surface
alone — the quantity consulted is the viewer's own, which `/v1/viewport` already serves — and closes
C31 for an indexed column once the set is warm.

**r3 (2026-09-02) — figures corrected against the implementation's bench.** The engine's suggestion
index and walk were built on branch `vs/engine`, and §6's shipped-route measurements replace r2's,
which had timed `Bitmap::intersect` with the record in hand and so understated the sparsest
viewer's keystroke by a factor of six to thirty; the counts and the extents sweep are measured where
r2 modelled or owed them.
Four deviations the implementation took are recorded at their claims: the index lives in the
engine's cache directory rather than a bundle runtime directory that does not exist, its payload
record is 12 B rather than the 4 B r2 costed, its rebuild is dispatched on a 4,096-value side-map
threshold rather than on every change, and `counts=true` composes a candidate on a `public` column
and refuses where it cannot.
