# Five filter rulings, and what each unblocks

**Date:** 2026-08-10 · **Status:** Escalation memo — evidence, not normative. Puts open questions
to the owner; rules nothing.
**Reads with:** [`2026-08-10-filter-handover.md`](2026-08-10-filter-handover.md) (§3 and §4, which
this expands), [`filter-index.md`](../../design/filter-index.md) (r7, Provisional),
[`filter-surface.md`](../../design/filter-surface.md).

The gate is green on `filter/index` as it stands: `cargo test --workspace` and
`cargo clippy --workspace --all-targets -D warnings` both exit 0, `check-layers.sh` passes, and
`check-doc-links.py` reports 0 errors / 14 warnings, all of them drifted line citations inside
frozen `probes/2026-07-31-concurrency-workstream/` reports and none of them filter work. Conformance
is at its stated baseline to the test — **3 failed / 81 passed / 1 skipped / 2 errors**, the
failures and errors confined to `test_overlay_journal.py` and `test_restart_replay.py`, which are
the pre-existing WAL and overlay ones and not filter work.

| | Ruling | Recommendation | Unblocks |
|---|---|---|---|
| **R1** | Defer `attrs/` digests to first touch? | Value columns yes, postings no | Open time at 10⁹; the reverted bounds check |
| **R2** | Where the fold's flip opens `FilterColumns` | Keep as built (post-flip) | Nothing — closes a stated conflict |
| **R3** | Isolate the scan in its own crate? | Not yet; A/B the two remaining items | The order of every §5 gap |
| **R4** | An r-letter for the contracts tree lines? | Annotate, no letter | A tidiness debt |
| **R5** | Surface §4's project-vs-per-tile rule | Build the per-tile route | `filter-index.md` promotion |

R3 is the one that changes the shape of the work that follows it; R5 is the one that gates
promotion. R2 and R4 are cheap and can be ruled in a line each.

---

## R1 — First-touch digest deferral for `attrs/`

**The question.** Bundle open hashes every file the manifest names, in full, before serving. Should
`attrs/` leave that sweep and be digested on first touch instead — and if so, all of it, or only the
value columns?

**What it costs today.** The sweep is O(bytes) and the attribute artefact is the largest thing that
has ever joined it: 4 GB per `u32` column at 10⁹, 8 GB per `i64`, which is the column's own size.
Sixteen declared columns is tens of gigabytes read and hashed before the first request. For a text
column it is worse than the arithmetic suggests, because it is the *first* of two full passes over
the same bytes — Arrow validates UTF-8 and offsets when the reader decodes the array, and probe
arm 15 measured that second pass touching every page (`RssFile` +472 MB on a 475 MB column,
`RssAnon` 0, ~40 ms per 475 MB).

**The precedent already exists, and so does the mechanism.** Contracts §0.3 deviation 9 defers the
external-ID sidecar's digests to first touch for exactly this reason: digesting at open reimposes
the sequential read the deviation exists to remove. Nothing is mapped, scanned or verified at open,
and `readyz` does not require it. In the code that is one predicate the open sweep consults per
file, skipping the bytes while still validating the path — so an unsafe manifest key cannot hide
behind the deferral. Extending it to `attrs/` is a change to that predicate plus a first-touch
check on the reader; it is not new machinery.

**The asymmetry that makes this rulable, and it splits the artefact in two.**

- A corrupt **value column** can only *narrow* `M_sel`. The scan runs inside the candidate — `M_auth`
  is pushed in first (§8.2) — so **I12** holds structurally: whatever garbage the column holds, an
  entity outside the candidate cannot be added to a result. The failure mode is a viewer seeing
  fewer items than they are entitled to, which is the fail-closed direction.
- A corrupt **posting** does not have that property any more. Under
  [decision 0060](../../decisions/0060-category-postings-serve-public-listings-and-never-per-viewer-ones.md)
  the derived postings feed `/v1/categories`' `per_viewer` visibility predicate. That is a
  disclosure control, and a disclosure control computed from unverified bytes is not obviously safe
  in either direction.

**Options.**

| | | |
|---|---|---|
| **a** | Defer the whole of `attrs/` | Largest win; puts a disclosure control behind unverified bytes until first touch |
| **b** | Defer `values.arrow` and `presence.roaring`; keep `postings.arrow` in the open sweep | Takes the term that dominates — postings are 6–12 MB per column against 4 GB — and leaves the disclosure control verified before serving |
| **c** | Defer nothing | Open time stays O(corpus bytes) per declared column |

**Recommended: (b).** It takes essentially all of the win, because the postings are three orders of
magnitude smaller than the columns they accelerate, and it leaves 0060's predicate resting on bytes
that were checked before anything was served. Under (b) the deferral is per file with the digest
checked on first touch *before* any borrowed view is constructed, and record-level validation moves
onto that same first-touch path — which is what actually guards the unsafe zero-copy view.

**What (b) obliges.** An amendment to **contracts §2.4** and to the shared reader's own safety
argument — the open-time validation is named in that module's discharge of an `unsafe` block, so
relocating it is a contracts change and not something a provisional filter design settles
(`filter-index.md` §8 says as much). Contracts §2.4 already records the amendment as owed.

**And it re-opens one reverted change.** The text-offset bounds check is implemented, tested and
reverted; it is redundant only while Arrow validates offsets on decode, and it cost **70% of the
scan** for R3's reason. If (a) or (b) lands, whoever restores it must re-derive whether it is still
redundant on the first-touch path and pay R3's A/B discipline to land it. That coupling is why R1
and R3 want ruling together.

---

## R2 — Where the fold's flip opens `FilterColumns`

**The question.** `filter-index.md` §6.2 places the `FilterColumns::open` in the post-flip rotation,
beside the postings reader and the external-ID sidecar. An implementer was separately briefed to
carry already-opened columns into the publication, so that nothing could fail *after* the manifest
edit. Those are opposite orders and the conflict was never resolved.

**What is built.** The document's order. `open_rotation` reads `CURRENT` first and **refuses unless
it names the prefix**, then opens the bundle, the postings, the external-ID index and the filter
columns over the new prefix. If any of those fails after the flip, the fold diverges from `CURRENT`,
logs an ALARM, serves the superseded prefix, publishes nothing and retires nothing; a restart opens
the folded bundle, which is complete on disc.

**Why the built order is not accidental.** The bundle identity *is* the digest `CURRENT` names, so a
rotation assembled before the flip is a rotation of a bundle that is not yet live — publishing it
would mean the process and its own storage disagreeing about which bundle is serving, with nothing
to detect it until a restart. And the four artefacts rotate as one value (`PrefixRotation`) precisely
so that no publication can rotate a *subset* of them; moving the filter columns to the other side of
the flip either splits that value or moves the postings and the sidecar with it.

**Options.**

| | | |
|---|---|---|
| **a** | Keep as built | A post-flip failure is a bounded ALARM-and-restart on a bundle that is complete on disc |
| **b** | Pre-open all four before the flip | Removes the restart window; costs the identity argument above, since there is no committed identity to open against yet |
| **c** | Pre-open to *validate*, discard, then flip and open again | Removes the window and keeps the identity argument; pays the open twice and lets the two opens disagree |

**Recommended: (a), and record it** — the ⊘ at §6.2 already describes the built behaviour, so this
ruling costs a sentence in the design saying the alternative was considered and why the identity
argument decides it. The failure it leaves is loud, bounded, and lands on a bundle that a restart
serves correctly, which is the fail-closed shape.

---

## R3 — The scan's crate isolation, and `codegen-units = 1`

**The question.** `tessera-filter`'s published constants are a property of the crate's *contents*,
not of the scan's code. Seven times in this work an unrelated addition moved them 30–70% — three
from code that never runs during a scan, once from a function that was never called (deleting the
call and leaving the symbol reproduced the regression exactly). Should the scan move into a crate
that holds nothing else?

**What the remedies measure (probe arm 16).** None is general. `#[inline(always)]` fixes the case in
front of you. One codegen unit fixes the **presence** path and not the **packing** path, and costs
the baseline ~15%. A crate boundary recovers about half — which is what put the write side in
`tessera-filter-write` (measured: writing the fold's pass inside `tessera-filter` cost the
universal-contiguous arm 0.27 → 0.44 ns per candidate entity at 10⁹, 65%, with the hot file
byte-identical). The scan-only crate is named at arm 16 as the structural answer, **unbuilt and
unpriced**.

**The input that decides it is how much work is left in that crate**, and it is two items: the
`MADV_SEQUENTIAL` hint through `ValueColumn::open`, and the reverted text-offset bounds check under
R1. Everything else outstanding is in `tessera-filter-write`, the engine, the server, the bench
matrix, or the documents. Two A/B campaigns — interleaved against a `HEAD` build, medians of three,
`layoutprobe`'s `realscan` and `textscan`, ~5% run-to-run drift — is a known cost. An unpriced crate
split is not.

**Options.**

| | | |
|---|---|---|
| **a** | Split now | Changes the cost of everything after it; unpriced, and the split itself must be A/B'd |
| **b** | `codegen-units = 1` as an interim | −15% baseline, bought against a sensitivity it only half removes. Paying a measured 15% to half-fix a hazard is the worst cell here |
| **c** | Neither; A/B the two remaining items | Two campaigns, no structural change, constants stay as published |

**Recommended: (c) now, and (a) the moment a third item wants that crate** — the split earns its
price when filter work becomes ongoing rather than closing out, and §2.2 already carries the warning
in the document where a reader will meet it. This is the ruling I would most like reversed by
information I do not have: if you expect the `text` type of §2.2, or `none_of`, or list attributes
to arrive within the next few tracks, (a) is right and should happen before them.

---

## R4 — An r-letter for the contracts tree lines

**The question.** Commit `34c0f11` added three lines to contracts §2.1's directory tree —
`coalesced/<id>/attrs/<column>/` with its `values.arrow` and `presence.roaring` — without minting a
revision letter. No field, no behaviour, no wire change; contracts is at r26 and §2.4's `attrs/`
entry is marked *(r25)*. The same branch's `730b919` added the `terms-0.dict` tree line the same way,
so there are two.

**Recommended: one annotation in Appendix R covering both, no revision letter.** Contracts already
has the form for this — the r6 entry carries an *"Annotated 2026-07-30 (design r22, no revision here
— no byte, schema or contract changes)"* note for exactly a change that made older text stale
without changing the format. A letter should mean a reader has to re-check something; neither of
these gives them anything to re-check.

---

## R5 — Surface §4's project-vs-per-tile rule

**The question.** Filters produce entity-space bitmaps; tiles are row space. `filter-surface.md` §4
states the rule as one line — *project the result only when it is smaller than about a quarter of the
viewport's row count; otherwise test membership per tile and project nothing* — and **only the
projecting route is built.** Is the rule ruled, and is the second route built?

**The measurement (arm 3, at 10⁹).** A projection costs ~27 ns per set bit and scales with the
**result**. A per-tile membership test costs ~6–22 ns per viewport row and scales with the
**viewport**, which the drawn-mark budget already bounds — so it scales with neither the corpus nor
how much the filter matched. The crossover is those two constants: a result of ~10⁵ against a
300,000-row viewport. Projecting a 10⁸-entity result costs **2,779 ms, more than the 730 ms scan that
produced it**; the narrow route is 3.8 ms at 10⁵.

**The caveat that has to ride the ruling.** The per-bit constant is shape-dependent and arm 3's
results are contiguous — the cheap end for a gather. The corpus's own scattered figure is 127 ns per
set bit (`probes/results.md` §6), ~4.7× arm 3's, which moves the crossover by roughly that factor.
A threshold hard-coded from the contiguous constant will project too much on scattered results.

**Options.**

| | | |
|---|---|---|
| **a** | Build the per-tile route with the measured crossover | Removes the dominant term for every broad filter; needs the shape caveat handled, conservatively or by measuring the scattered constant first |
| **b** | Projection only, and say so at the claim | Every broad filter pays a projection larger than its own scan — this is a viewport-path cost, and §10.4's warning about `Permutation::project` drifting onto the per-viewport path is unambiguous |

**Recommended: (a)**, with the threshold set from the *scattered* constant rather than the
contiguous one until a scattered arm runs, so the rule errs toward the route whose cost is bounded
by the viewport.

**One correction to make either way.** §4's closing paragraphs still assert that a filter operand's
projection is principal-independent and "computed once and shared across every principal". The ⊘
note immediately above them withdraws exactly that premise — under `filter-index.md` §2.2 the mask
is the scan's candidate, so a result is `M_sel` and principal-*specific*. The stale paragraphs sit
below the note that supersedes them and read as current.

---

## Not rulings — the rest of what promotion needs

`filter-index.md` is Provisional at r7 with its adversarial round dispositioned. Beyond R5, one
measurement stands between it and normative: **§2's constants confirmed at a value width other than
`u32` and on a string column** — the campaign swept `u32` and text separately and never crossed
them. That is work, not a decision.

Two amendments are owed to **normative** documents and should ride their own reviews rather than
this one: compaction §2's table and §3's pass list gain the attribute pass and the band budget;
write-path §7 and contracts §2.1 gain the coalesce's fourth axis.

**And one thing that is neither, found while checking the above: the decision numbering has forked.**
This branch carries `0059-filters-compose-as-a-boolean-tree-inside-the-candidate.md` and
`0060-category-postings-serve-public-listings-and-never-per-viewer-ones.md`, and has no 0058.
`client/replica-cache` carries `0058-a-single-flight-racer-waits-rather-than-being-refused.md` and a
**different** 0059, `0059-per-principal-admission-is-not-capped.md`. Both survive a merge — the
filenames differ — leaving two decisions numbered 0059 in an immutable, one-per-file set that is
cited by number throughout the corpus. Renumbering one of them costs a rename and its inbound
citations, and costs far more once either number is cited from a promoted document.
