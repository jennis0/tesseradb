# Five filter rulings, and what each unblocks

**Date:** 2026-08-10 · **Status:** Ruled by the owner, 2026-08-10. Evidence, not normative — the
rulings bind, and each is recorded at the section that put it. Where a ruling changes a design, the
design carries it and this memo is the account of why.
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

| | Ruling | **Ruled** | Unblocks |
|---|---|---|---|
| **R1** | Defer `attrs/` digests to first touch? | **Value columns yes, postings no** (option b) | Open time at 10⁹; the reverted bounds check |
| **R2** | Where the fold's flip opens `FilterColumns` | **Keep as built** — post-flip (option a) | Nothing — closes a stated conflict |
| **R3** | Isolate the scan in its own crate? | **Neither, yet — the cause is under investigation.** The owner's reading is that a never-called function moving a constant 65% indicates something is wrong rather than something to route around, and no remedy is chosen before the mechanism is known | The order of every §5 gap |
| **R4** | An r-letter for the contracts tree lines? | **Leave it** (option c) — no letter, no annotation | Nothing |
| **R5** | Surface §4's project-vs-per-tile rule | **Build the per-tile route** (option a) — **and two further routes the viewport wants, which this memo did not anticipate** (below) | `filter-index.md` promotion |

R3 is now an investigation rather than a choice, and it still gates the order of the work that
follows it. R5 grew: the two-route rule stands, and the surface needs two more routes beside it.

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
  [decision 0061](../../decisions/0061-category-postings-serve-public-listings-and-never-per-viewer-ones.md)
  the derived postings feed `/v1/categories`' `per_viewer` visibility predicate. That is a
  disclosure control, and a disclosure control computed from unverified bytes is not obviously safe
  in either direction.

**Options.**

| | | |
|---|---|---|
| **a** | Defer the whole of `attrs/` | Largest win; puts a disclosure control behind unverified bytes until first touch |
| **b** | Defer `values.arrow` and `presence.roaring`; keep `postings.arrow` in the open sweep | Takes the term that dominates — postings are 6–12 MB per column against 4 GB — and leaves the disclosure control verified before serving |
| **c** | Defer nothing | Open time stays O(corpus bytes) per declared column |

**Ruled: (b)** (owner, 2026-08-10). It takes essentially all of the win, because the postings are three orders of
magnitude smaller than the columns they accelerate, and it leaves 0061's predicate resting on bytes
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

**Ruled: (a), and recorded** (owner, 2026-08-10) — the ⊘ at §6.2 already describes the built
behaviour, so this ruling costs a sentence in the design saying the alternative was considered and
why the identity argument decides it. The failure it leaves is loud, bounded, and lands on a bundle
that a restart serves correctly, which is the fail-closed shape. The briefing that said otherwise is
superseded; nothing in the code changes.

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

**Ruled: none of the three** (owner, 2026-08-10) — all three route *around* the phenomenon, and the
owner's reading was that the phenomenon itself was the finding. **The investigation settled it the
next day** ([`scan-constant-sensitivity`](2026-08-11-scan-constant-sensitivity.md)), and the
suspicion was right on both counts:

- **The cause is instruction-address alignment, not code generation.** The perturbed build emits the
  hot functions instruction-for-instruction identical and places them 0x50 bytes elsewhere; padding
  the crate's text with inert bytes reproduces the whole effect as a function of shift mod 64.
- **~0.27 ns is not the scan's cost.** It is one draw from a bimodal distribution — ~0.25–0.28 or
  ~0.42–0.44 ns, nothing between — and it is the favourable one. §2.2 now says so.
- **A crate boundary would have re-rolled the layout rather than pinned it**, so option (a) was the
  wrong answer and "recovers about half" was luck. §6.2's performance rationale for the
  `tessera-filter-write` split is withdrawn; the split stands on audit separation.

⊘ **What remains is a profile decision, unmade:** `-C llvm-args=-align-all-functions=6` pins
function starts to 64 bytes at no measurable baseline cost, and is independently confirmed to put
all four hot symbols on residue 0 where unpinned they sit at 16, 0, 48, 0. It was measured on
`realscan` alone and wants re-running against the real `tessera` binary, on a quiet machine, before
the workspace profile is changed.

---

## R4 — An r-letter for the contracts tree lines

**The question.** Commit `34c0f11` added three lines to contracts §2.1's directory tree —
`coalesced/<id>/attrs/<column>/` with its `values.arrow` and `presence.roaring` — without minting a
revision letter. No field, no behaviour, no wire change; contracts is at r26 and §2.4's `attrs/`
entry is marked *(r25)*. The same branch's `730b919` added the `terms-0.dict` tree line the same way,
so there are two.

**Ruled: leave it** (owner, 2026-08-10) — no revision letter, and no Appendix R annotation either.
A letter should mean a reader has to re-check something, and neither commit gives them anything to
re-check; a note recording that nothing needs re-checking is the revision archaeology the house
style exists to keep out of the corpus. The tree lines stand as ordinary description of where the
files are.

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

**Ruled: (a)** (owner, 2026-08-10), with the threshold set from the *scattered* constant rather than
the contiguous one until a scattered arm runs, so the rule errs toward the route whose cost is
bounded by the viewport.

**One correction to make either way.** §4's closing paragraphs still assert that a filter operand's
projection is principal-independent and "computed once and shared across every principal". The ⊘
note immediately above them withdraws exactly that premise — under `filter-index.md` §2.2 the mask
is the scan's candidate, so a result is `M_sel` and principal-*specific*. The stale paragraphs sit
below the note that supersedes them and read as current.

### R5b — Two further routes the viewport wants, and what they turn on

**Owner requirement, 2026-08-10.** The two routes above both answer *"which entities match?"* and
hand back a set. A viewport wants two questions neither of them asks, and both exist to support
**in-screen filtering and highlighted subsets** — changing what is emphasised without refetching the
map:

3. **Re-test what the client already holds.** The client has points from a previous request, keyed
   on its region and *k*, and asks which of them pass a filter.
4. **Annotate a fresh sample.** The client asks for points as usual and receives, per point, a 0/1
   saying whether it passed the filter.

Both are cheap in the shape the scan already has — the candidate is the *sample*, tens to hundreds
of entities, the smallest candidate the scan will ever see — and route 4 is the per-tile route of
(a) with the per-point verdict *retained* rather than intersected away. Neither needs a new
structure.

**And because the selection is deterministic, these are one route with two payload modes rather
than two routes.** Both compute the same thing: re-derive the sample for `(region, k)` at the
current generation, evaluate the operand over it, and return a verdict per point. They differ only
in whether the points travel with the verdicts (route 4, for a fresh view) or are assumed already
held (route 3, which is route 4 minus the payload). Specifying them as one thing is what stops the
two from drifting into two selections that disagree.

What they need is a specification, because they are a change to the served surface and three things
about them are decisions rather than details:

- **The held set is named by `(region, k, generation)`, and the server re-derives it.** The
  selection is a pure function of the visible mask and the tile: §7.2 takes the visible row ids in
  the tile's range straight from the bitmap and keeps the lowest by identity, and priority is a
  keyed per-point constant that is *mask-independent* — so the same viewport and *k* against the
  same generation and the same idset yield exactly the same set. The generation belongs in the key
  because that is the whole of "up to an update": a flush, a merge, a fold or an accepted
  suppression is what changes the answer, and the corpus already has the vocabulary for saying so
  (the staleness stamp, [decision 0041](../../decisions/0041-pins-become-a-staleness-stamp.md), and
  the view key, [decision 0029](../../decisions/0029-view-key.md)).

  **[Decision 0030](../../decisions/0030-determinism-is-not-a-guarantee.md) does not stand in the
  way of this, and reading it as though it did is the error to avoid.** What it declines to promise
  is byte-stable *ordering and encoding* — "two byte-different encodings of the same served set are
  equally correct" — not which set is served. Membership is deterministic by construction; the
  sequence it arrives in is not promised. **What that does forbid is a bare positional reply**: a
  flags-only payload aligned by index to what the client holds depends on the ordering 0030 declines
  to guarantee, so a future reordering would silently mislabel every point. The verdicts are keyed
  by `tessera_id`. Route 4 is unaffected — its column travels beside the points in one response and
  is self-aligned.
- **The server must re-derive `M_auth` and never trust the claim to hold a point.** A `tessera_id`
  presented back is a claim about the past, and [decision 0041](../../decisions/0041-pins-become-a-staleness-stamp.md)
  is unambiguous that a suppression applies to every request the moment it is accepted, whatever
  stamp was presented. So a point held from before a suppression must come back as *not visible*,
  not as a filter verdict — the two outcomes have to be distinguishable in the response and the
  fail-closed one has to be the default.
- **In route 4 the filter is a reported column and never a predicate on selection.** The sample is
  the one an unfiltered request would return — drawn from `M_auth`, with `k` marks chosen exactly as
  §7.2 chooses them — and the filter contributes one boolean per returned point and nothing else.
  **I7** is untouched, because the filter never enters the sampling step at all. Implementing this
  as "filter, then sample" or "sample, then top up with matches" would be a different feature and a
  worse one.

  **The consequence to state at the claim** is a product one rather than a security one: route 4
  answers *"of the marks you would see anyway, which pass"*, so a filter matching a rare value can
  highlight nothing in a tile that genuinely contains matches — the selection had no reason to
  prefer them. That is the correct behaviour for shading a map the viewer is already looking at, and
  the wrong tool for finding where the matches are. Route (a)'s filtered viewport is what answers
  the second question, and the surface should make it obvious which is which.

**On the invariants, the first read is that both are inside the line, and it should be checked
rather than taken.** The flag is a property of a point already inside `M_auth`, so it is computable
from inside `M_auth` alone (**I2**). The frontier is unaffected: route 4 computes it on `M_auth` as
an unfiltered request does, so the filter moves it neither up nor down, which is stricter than
**I12** requires. And the selectivity a client could estimate by counting flags is a quantity the
filtered route already publishes exactly (§5.3's masked counts), which is
[decision 0023](../../decisions/0023-derivable-quantities-are-not-disclosures.md)'s shape — but that
is an argument to be made in the leak register, not asserted in a memo.

**And the timing property has to survive both.** `filter-index.md` §2.2's guarantee is that the work
is a function of `(candidate, column)` and never of the value, so a value the principal cannot see
costs what an absent value costs. That holds for routes 3 and 4 only if they run the *same*
traversal with a different sink. Implementing either as a special case is how it would be lost.

---

## Not rulings — the rest of what promotion needs

`filter-index.md` is Provisional at r7 with its adversarial round dispositioned. Beyond R5, one
measurement stands between it and normative: **§2's constants confirmed at a value width other than
`u32` and on a string column** — the campaign swept `u32` and text separately and never crossed
them. That is work, not a decision.

Two amendments are owed to **normative** documents and should ride their own reviews rather than
this one: compaction §2's table and §3's pass list gain the attribute pass and the band budget;
write-path §7 and contracts §2.1 gain the coalesce's fourth axis.

**The decision numbering had forked, and is fixed.** `main` ends at 0057. Both this branch and
`client/replica-cache` then minted 0058 onwards independently on the same day, so a merge would have
produced two different decisions numbered 0059 in a set that is immutable and cited by number. This
branch's two moved up — filters-compose 0059 → **0060**, category-postings 0060 → **0061** — leaving
0058 and 0059 to `client/replica-cache`, which holds them already and is where renumbering would
have meant editing another branch's uncommitted work. The `docs/decisions/README.md` table shows the
gap at 0058–0059 until that branch merges, which is the honest state rather than a defect.
