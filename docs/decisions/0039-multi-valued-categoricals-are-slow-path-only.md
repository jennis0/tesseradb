# 0039 — Multi-valued categoricals are a slow-path shape; there is no render projection

**Date:** 2026-08-02 · **Status:** Settled

## Context

`per-point-attributes.md` §3.7 refuses `render` on a multi-valued attribute, on the ground that a
rendered mark has one colour. It also defers multi-valued attributes entirely, and gives as the
reason that they "have no hot-path component".

That second clause is what gets re-litigated. It reads as an empirical claim about encodings rather
than a rule, so it invites the reply "here is an encoding that *does* give them one" — and the real
arXiv corpus is exactly the case that provokes it: `categories` is multi-valued but small
(176 distinct, mean 1.72), which looks like it ought to fit in a row somewhere.

## Decision

**Multi-valued is a slow-path shape. It is admissible under `filter` and `inspect`, and never
under `render`.** No projection, derived value or summary of a multi-valued attribute earns a hot
column on its behalf.

A caller who wants to colour by a value drawn from a multi-valued field declares an **ordinary
single-valued attribute** carrying that value. That is not a workaround: it is the caller saying
which single value they mean, in a column that means exactly that, with no mechanism between the
declaration and the row.

The rejected alternative was `render = "primary"` — sugar compiling a multi-valued declaration into
a single-valued column holding the first-listed value. It was rejected for costing a derived
placement that can drift from what the caller meant, to save a line of declaration.

## Why not an encoding

Measured on the full corpus (`data/corpus.parquet`, 2,422,486 rows), not on a sample — the sample
figures that motivated several of these are materially lighter and should not be quoted:
`categories` 176 distinct, mean 1.72, max 13, **80,902** distinct whole-value combinations;
`surnames` 404,104 distinct, mean 4.54, max **2,832**, with 138,861 singletons.

Each row-space encoding fails for its own reason, recorded so none is rediscovered:

- **Fixed *k* slots** — the slots past the first have no reader. Render draws one colour; filtering
  row-wise duplicates the postings and bypasses the mask, which §10.4 makes the sole entry point;
  and counting from gathered rows counts a **sample**, since §7.2 selects after masking. Paying
  residency for bytes nothing reads. Insufficient anyway: four slots cover 59.8% of surname
  (item, value) pairs (measured).
- **Per-row vocabulary bitset** — 32 B/row for a 256-value vocabulary is 29.8 GiB at 10⁹
  (arithmetic against §10.5), more than doubling `columns.arrow`. And the bit *position is the
  code*, so the wire ships set bits at the positions of values the principal cannot see.
- **Dictionary of value combinations** — refuted by measurement: 80,902 combinations at 0.24% of
  target scale, open-ended under streaming ingest. Projecting a combination per viewer would also
  make the gather a per-viewer rewrite rather than a copy.
- **A "has more values" bit or count byte** — **the one that looks free and is a disclosure.** The
  bit is computed over the full value set and baked into the row, so it cannot be per-viewer. A
  principal who establishes that their only visible value on a point is *v*, and then reads
  `has_more = 1`, has learned that the point carries at least one value they cannot see: a
  per-item lower bound on invisible values, one bit per point. That is precisely the property
  §3.4's scattered codes exist to deny, relocated from vocabulary space to item space. "How many
  more" is an `inspect` question, where it is gated.
- **A membership sketch or fingerprint** — bits set by invisible values are observable, and a
  client-queryable membership structure over values the principal cannot name is a quantity derived
  from outside `M_auth` and then gated, which **I2** forbids.

## Consequences

§3.7's rule is unchanged; its stated reason is corrected. The filter and inspect placements are the
whole of a multi-valued attribute's presence, and that is a rule about placement rather than a
claim about what encodings exist.

`surnames` gets no hot-path presence under any declaration — no legend exists at 404,104 values, so
colouring by it is not a render use, and "papers by these authors" is a filter, served exactly and
in entity space by the postings.

Lifting the `multi = true` parse refusal for `filter` and `inspect` remains open work. This decision
fixes what that work may not do.
