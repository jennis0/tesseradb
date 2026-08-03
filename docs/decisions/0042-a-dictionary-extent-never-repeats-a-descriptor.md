# 0042 — A dictionary extent never repeats a descriptor, and the loader enforces it

**Date:** 2026-08-03 · **Status:** Settled

## Context

A term's ordinal is its **position** in the concatenation of `dict_extents` in listed order. Nothing
in an extent file records an ordinal; `Dict::load` walks the files in order with one running counter,
and that counter is the whole of the mapping.

Two functions build a dictionary, and they disagreed about a descriptor an extent repeats.
`Dict::load_extending` — the in-memory path, used when a flush publishes — skipped a descriptor the
base already carried. `Dict::load` — the restart path — inserted every record and advanced the
counter for it. Measured, at the time promotion was designed:

| | `a` | `b` | `len` |
|---|---|---|---|
| in memory, `load(base).load_extending(ext)` | 0 | 1 | 2 |
| after restart, `load([base, ext])` | **1** | 2 | 3 |

Not only the repeated descriptor moves: **every ordinal after it shifts by one.**

The consequence is a cross-compartment disclosure. A delta tier's postings are written under the
in-memory ordinals. After a restart the same bytes are read under the other numbering, so a session
granted one descriptor is served the items of whichever descriptor now occupies its ordinal — with
no error, no log line and no degradation anyone could notice.

It was unreachable while it stood: `flush::promote` was a no-op, so no bundle had ever carried a
second dictionary extent. Descriptor promotion is the change that writes one.

## Decision

**Both sides enforce the rule, and the format owns it.**

- **Writer.** A flush resolves each novel descriptor against the live dictionary before interning
  anything. A descriptor already present takes the ordinal it already has and contributes no record.
- **Reader.** `Dict::load` skips a descriptor it already holds and does not advance the counter for
  it, so `load(a ++ b) ≡ load(a).load_extending(b)` holds for every input, including inputs no
  correct writer produces. `Dict::len` becomes the count of distinct descriptors rather than of
  records — a difference only in the case the writer rule forbids.
- **Contracts §2.4** states both as format rules: `dict_extents` is positional and append-only, a
  contiguous run may be coalesced but never permuted, and no descriptor may appear twice across the
  list.

## Why both, when either would do

The usual answer here is to enforce a rule in one place and cite it in the other. This is the
exception, and the asymmetry is the argument: the writer rule is a property of **one caller**, and
the reader rule is a property of **the artefact**. A merge coalescing extents, a compaction
renumbering the dictionary, a future importer — each is a new writer that would have to rediscover
the rule from prose, and the failure it guards is silent and cross-compartment. A reader that cannot
be made to disagree with itself is worth more than the line it costs.

## Consequences

- Restart equality is a conformance obligation with a test at the loader (`load(a ++ b) ≡
  load(a).load_extending(b)`, including a repeat) and one at the flush (a reopened bundle resolves
  every promoted descriptor to the ordinal the publishing process assigned).
- The flush also asserts that its extent contains no descriptor the dictionary already holds,
  **read off the file** rather than inferred from lookups — a duplicate is invisible to `lookup` in
  the process that wrote it, and only appears after a restart.
- A compaction that renumbers the dictionary inherits the same rule and the monotone-length
  obligation §3.3's staleness hint rests on.

## Provenance

`docs/evidence/memos/2026-08-03-descriptor-promotion-design.md` §3, where the divergence was found
and measured while designing descriptor promotion.
