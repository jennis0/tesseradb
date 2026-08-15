# Identity and storage scaling — an options analysis

**Date:** 2026-08-05
**Status:** Evidence — an options analysis, **never normative**. Nothing here changes a design or
a decision; where an option would need one, the ruling it needs is named. Every claim about the
tree was verified against branch `geometry/cell-plus-residual` at `673c267` (plus the uncommitted
scale harness and this week's memo). Every figure is marked **measured**, **modelled** or
**assumed**; a figure quoted from a probe should be re-run before it is relied on.

**Commissioned on four questions:** (1) how the system should manage scaling and storage over
time — merges, deletions, and what degrades under sustained ingest; (2) whether the global ID
scheme is the right one, end to end; (3) whether compaction can reach the quality of a
from-scratch build without compromising uptime; (4) whether anything would make `tessera_id`
truly stable without breaking the rest of the system.

---

## 0. Answers first

**Q1 — storage over time.** The write-path machinery (window → flush → merge → coalesce) is
sound and measured; three of its four growth axes are bounded repeatedly and indefinitely. Two
real gaps remained when this ran: **the fold did not exist** (nothing retired, nothing reclaimed —
the then-provisional `compaction.md` answered it but carried three undispositioned fatal findings;
it is normative and built since, and those findings are dispositioned), and **the segment
axis is unbounded in the shipped configuration** — merge saturates at tier 2 (~149 MiB) and the
live segment count then grows at ~1 per 4M rows ingested, without limit (measured, §1.2 below).
The first is the roadmap's known debt; the second needs a one-line ruling nobody has made.

**Q2 — global IDs.** Keep the scheme. The four-space factoring (external ID / entity ID / row ID
/ `tessera_id`, plus bundle-relative term IDs and never-reused `seg_id`s) is coherent end to end,
and each boundary earns its place. `u32` exhaustion is **not a live concern for the target's
ordinary operation** (§2 arithmetic) but **is** a concern under whole-corpus re-label churn,
because decision 0047 makes every edit burn an entity ID; the escape (per-shard allocation under
the reserved 32/32 split, at a format bump) is already designed into the identity layout. The one
genuine defect in the scheme is the known one: `entity_id` holds two jobs — permanent identity
and index position — with contradictory requirements. §4 re-prices the recorded fix.

**Q3 — compaction versus a from-scratch build.** **Yes on three of the four quality axes, and
provably not on the fourth — with the fourth bounded and separately purchasable.** The fold as
designed reaches full-build quality in row space (one segment, globally Morton-sorted, byte-exact
codes), in reclamation (disc returns to ~1× live bytes from the measured 1.3–1.6× steady state),
and in deny hygiene (Rule F retirement, overlay bounded). The axis no fold can reach **within the
identity guarantee** is entity-axis contiguity: I9 forbids renumbering, so postings remain a
concatenation of per-window-sorted runs for ever, and the un-banked win stays un-banked
(measured ceiling 8.9–36.7× on posting bytes, up to ~130× on union cost; measured floor: today's
authorise worst case of 588 ms at 10⁹ *is already* the fragmented state, so nothing degrades from
published figures). That axis is closed only by the index-ordinal split (§4) or by an
identity-breaking rebuild. On uptime: the fold fits decision 0044's budget even at its floor —
D1's third arm ("multisecond delays much rarer than the flush/merge rate") is satisfied by a
daily-class fold against a 90 s tick — and the pre-swap warm, if repaired, collapses the window
to the swap. The honest floor if the warm cannot be repaired is stated in §3.3.

**Q4 — a truly stable `tessera_id`.** **No, and the "no" is a ruling already made, not a cost.**
Absolute stability across key rotation requires either persisting a cross-idset translation —
which re-opens the C20 probe channel that decision 0025 exists to dissolve — or never rotating,
which deletes the only recovery from a compromised blinding key (decision 0014: the mixer is
non-cryptographic, so rotation is the mitigation of record). What *is* achievable, cheaply, is
removing every other instability: the ι split removes the identity-breaking rebuild as the only
route to index quality, and the §12.5 idset advance on repartitioning looks removable under the
global allocator (§5). That leaves exactly one break event — deliberate rotation — and
`external_id` remains the persistence identity, as decisions 0003/0005 already say.

**Ranked recommendation** (full table and cost-if-wrong in §6):

1. **Disposition `compaction.md`'s r3 findings and build the fold** — the only option that
   discharges Rule F, reclamation and the segment axis at once. Take the post-swap refresh with
   the 429 residual as the acceptable floor; repair the pre-swap warm as an optimisation, not a
   precondition.
2. **Rule the segment axis now** — either raise `max_merged_segment_bytes` (with the measured
   4.4–4.9× memory multiplier stated at the knob) or add tier-saturation to the fold's trigger
   gauges. One paragraph of ruling; without it a year of sustained ingest is ~360 live segments.
3. **Re-record the ι sketch with §4's corrected pricing** — one new array, not two; the overlay
   objection dissolved by the deny mask; the fold as its natural execution site. Keep it
   **deferred** behind its existing trigger; do not build it now.
4. **Do not pursue absolute `tessera_id` stability.** Optionally rule that a repartitioning
   preserves entity IDs (§5), which makes the identifier stable across everything but rotation.

---

## 1. Q1 — what degrades under sustained ingest, and who bounds it

The inventory, one row per growth axis. "Bound" means bounded repeatedly under sustained ingest,
not bounded once.

| Axis | Grows with | Bounded by | State | Evidence class |
|---|---|---|---|---|
| ingest buffer | arrivals per tick | flush (90 s tick) | built, measured | measured — flush 0.42–0.58 s at 10⁷ (`2026-08-05-write-path-at-scale.md` §1) |
| segments | flushes | merge — **saturates**; then nothing | **unbounded as shipped** (§1.2) | measured — 2→18 over 200 rounds, +1/~4M rows |
| delta tiers | flushes | coalesce | built; bounds repeatedly (8→2, four times over 32 rounds) | measured |
| external-id runs / locator extents | flushes | coalesce | built; bounded | measured (same harness) |
| dictionary extents | promoting flushes | coalesce | built; bounded | measured |
| dictionary length | novel descriptors | nothing — monotone by design (ordinals are positions; the staleness hint's counter) | FST ratified, unbuilt: 0.78 GB vs 7.09 GB at 1.17×10⁸ terms; the 7.1 GB clone per promoting flush and the 40–53 s open rebuild are the live costs | measured (`probes/2026-08-03-dict-fst/`) |
| WAL | writes since rotation | rotation (growth-gated) | built | — |
| on-disc dead bytes | merges + coalesces | **nothing — the fold** | 1.32× (250M run) to 1.59× (5M run) of live bytes, monotone; **note** the corrected figures — the memo's own §2 correction supersedes the 2.0–2.6× band still quoted in its closing paragraph and in `compaction.md` | measured |
| overlay (`deleted`) | deletions ever accepted | **nothing — the fold** (Rule F) | grows monotonically; per-request cost is O(1) (`Generation::denied`, one `andnot`) but per-*publication* cost is O(deleted ∪ suppressed) `row_of` lookups at every geometry publication, and manifest `tombstones`/WAL-snapshot bytes grow with it (modelled 30–60 MB, hundreds of ms at 10⁶ entries) | measured structure; modelled costs |
| tombstoned rows | deletions | **nothing — the fold** | rows stay; invisible (postings + deny mask), so they cost range span (a C4-shape timing residual), disc and residency, never counts | verified in code (§7) |
| posting fragmentation across windows | commit windows ever closed | **nothing — not even the fold** (I9) | monotone in window count; §11.1's un-banked win. The floor is already the measured state: 588 ms authorise worst case at 10⁹ is a created-order figure | measured floor; modelled window-scope runs (~10¹) |
| entity-ID holes | deletions + 0047 edits | nothing (identity guarantee) | 4 B/hole in `permutation.bin` + 4 B/hole in `ext-locator.u32`; linear, cheap; the real budget they consume is §16's u32 headroom | arithmetic from contract |

**What follows.** The system's write path does not degrade in any axis it was designed to bound;
every unbounded axis above is one of the three obligations `compaction.md` §0 already assigns to
the fold — plus the segment axis, which is a configuration ruling, and posting fragmentation,
which is §4's separate question. Q1's answer is therefore not a new mechanism: it is *finish the
fold, rule the segment cap, and leave the rest alone*.

### 1.1 The fold is the answer to three axes at once, and to nothing else

Rule F retirement, reclamation, and reorganisation-at-the-root are the fold's three obligations
(`compaction.md` §0), and no cheaper mechanism reaches any of them: retirement without a postings
fold is fail-open (a post-retirement fragment would still contain the entity — architecture §11.3
r33), reclamation without a prefix flip is impossible (every side-manifest below `n` names the
orphans), and merge is structurally barred from the base segment (twice over, write-path §7).
The stamp-ledger alternative for early retirement was deleted as "precision nothing pays for"
(write-path §5.4) and nothing here re-opens it.

### 1.2 The segment axis needs a ruling, not a mechanism

Measured (`2026-08-05-write-path-at-scale.md` §4): `MergePolicy::select`'s total-under-cap rule
means a ~149 MiB tier-2 segment can never be merged again at the shipped 256 MiB cap, so live
segments grow at ~1 per 4M rows — ~60/day at a 90 s tick and 250k-row flushes. Viewport cost is
measured flat per (tile × segment) at 1.4–1.6 µs, so request cost tracks segment count linearly
and without bound. Three resolutions, cheapest first:

- **Raise the cap** (a config default, no code): a 1 GiB cap buys two more tiers; its price is
  the measured 4.4–4.9× pool transient (≈4.4–4.9 GB at 1 GiB inputs) and it still saturates,
  later. Honest but temporary.
- **Let the fold be the backstop**: add segment count (or tier saturation) to the fold's trigger
  gauges. The fold returns to one segment per partition-slice by construction. This is the right
  long-term shape and costs one gauge.
- **Let merge consume the base** — rejected: it is compaction under another name, pays the full
  permutation rewrite, and write-path §7 excludes it twice for stated reasons.

Recommendation: both of the first two — raise the default modestly with the multiplier stated at
the knob, and give the fold the gauge.

---

## 2. Q2 — the global ID scheme, end to end

The chain, and the verdict on each link (all verified in the tree):

- **`external_id`** — caller-owned boundary identity, ≤64 bytes, sidecar + locator, newest
  binding wins (0003, 0047, contracts §2.4). Right: it is the only identity the caller may
  persist, and the store is a translation table, not an identity store.
- **`entity_id`** — `u32`, single global allocator, append-only, never reused (I9), never
  exposed (I10), signature-sorted within one commit window (§11.1), refused at `u32::MAX`
  (`Allocator::try_new`, verified). Right as identity; overloaded as index position — §4.
- **`row_id`** — `u32` per-slice Morton rank, related to entity space only by `permutation.bin`
  + extents (I4, verified: the dispatch lives in `tessera-store/src/permutation.rs` and `row_of`
  / `project` are the only crossings). Right, and load-bearing for everything.
- **`tessera_id`** — `u64 = Feistel₈,ₖ(shard_id ‖ entity_id)`, `splitmix64` rounds, key in
  MANIFEST, blinding not encryption (0005, 0014; `tessera-types/src/identity.rs` verified
  against the vectors). Right, with the threat model stated exactly.
- **`term_id`** — bundle-relative ordinals, renumbering made safe by the fragment-cache key
  carrying the postings identity (r19). Right, and the precedent the ι sketch leans on.
- **`seg_id`** — never reused; ABA safety for merge/coalesce publication. Right.

**Exhaustion arithmetic** (all arithmetic, not measurement). Headroom is 2³² ≈ 4.29×10⁹. At the
10⁹ design target: ~3.3×10⁹ spare. Group-commit allocation issues exactly what it allocates (no
slack — §16 r23), so consumption is ingest plus churn. Decision 0047 makes **every edit cost one
entity ID** (delete + re-ingest, the old ID burned for ever). At 10% annual edit churn over 10⁹
live items that is 10⁸ IDs/year — ~33 years of headroom; at 100% annual churn, ~3.3 years; **a
whole-corpus re-label campaign costs 10⁹ IDs per pass, so three passes exhaust the space.** The
consumed-versus-live gap also widens `permutation.bin` and `ext-locator.u32` at 4 B/hole each —
linear and affordable — and it is the ID budget, not the array width, that binds first.

Verdict: **not a live concern at the target for ordinary operation; a real concern under bulk
re-labelling.** Two mitigations, both already in the design: the 32/32 `(shard_id, entity_id)`
split in the identity input is exactly the escape (per-shard allocation at a format bump —
contracts §1 says so), and it co-occurs with the other 32-bit walls (Morton grid widens past
~4×10⁹; row IDs and standard Roaring are u32), so the format bump is a single event, not a
per-structure scramble. One cheap addition worth making now: publish consumed-versus-live on
`/control/status` beside the fragmentation gauge, so the burn rate is a number rather than a
surprise.

---

## 3. Q3 — can compaction reach from-scratch quality without compromising uptime?

### 3.1 The four quality axes of a from-scratch build

| Axis | Full build | The fold (as designed) | Gap |
|---|---|---|---|
| Row space | one segment per partition-slice, globally Morton-sorted, dense row IDs, no dead rows | **identical** — pass 1 is a k-way merge into exactly that, byte-exact through the code | none |
| Postings | one base tier, no tombstoned entities | **identical in content** — pass 2 folds tiers in and tombstones out | none in content |
| Entity-axis contiguity | entity IDs assigned in **global** signature order → postings maximally contiguous | **unreachable** — I9 forbids renumbering; postings stay per-window-sorted runs over a globally unsorted axis | the whole of §11.1's un-banked win |
| Identity-space density | fresh IDs, no holes | holes persist (4 B each in two arrays; u32 budget) | bounded, linear |

Plus the dictionary, where the fold is deliberately *better* than a build for uptime (carried
verbatim, no 7.1 GB clone, no 40–53 s rebuild) at the cost of never shrinking — bounded by term
cardinality, which the FST makes cheap in bytes.

So the precise answer: **the fold reaches from-scratch quality on every axis a fold is allowed to
touch.** The one axis it cannot touch is fenced off by I9, not by cost, and its size is known and
policy-dependent: 8.9–36.7× on posting bytes and up to ~130× on union cost as the measured
ceiling (simulated global re-sort; 1.0× for the `surnames` policy — it can be worth nothing),
with today's published authorise figures *already* being the fragmented state (588 ms realistic
worst case at 10⁹, measured — nothing regresses; the question is only what is never collected).
§4 is the option that collects it.

### 3.2 What the fold costs a running deployment, honestly

- **Wall clock:** modelled minutes-to-hours at 10⁹, IO-bound streaming; unmeasured (P1 named in
  `compaction.md` §14).
- **Disc:** peak ~2× live bytes (old + new prefix), then reclamation to ~1×.
- **Memory:** designed O(1) in corpus size; **r3 found the claim false at named places in
  existing code** (no cheap in-process second-prefix open; no mmap permutation writer). This is
  engineering, not architecture — the constructions exist elsewhere in the tree
  (`PostingsSpool`, `Permutation::load`'s page-cache posture) — but it is not done.
- **The viewer:** every row-space artefact in the process is invalidated (a fold permutes row
  space globally — the rank-shift shortcut was attacked in review and failed, correctly). Every
  resident session needs a full projection rebuild (measured 4 550 ms primitive / 10.7 s end to
  end at 10⁹) and a fragment rebuild (measured ~200 ms, flat in tier count).
- **Page cache:** the fold streams the bundle past it; effect on concurrent viewports assumed
  benign, and that is the design's weakest assumption (P3).

### 3.3 The uptime floor, if the pre-swap warm stays broken

r3 refuted the pre-swap refresh on four independent mechanisms (cache byte-bound eviction, the
prefix-skipping refresh, the watermark-hashing fragment key against a 90 s tick, and the loss of
`refresh_in_flight`). Suppose it cannot be repaired. The floor is the ordinary post-swap refresh:
`refresh_in_flight` armed, resident entries rebuilt on the pool (~16 wide-grant entries × 4.55 s
≈ ~73 s of pool time at a 2 GiB cache bound; arithmetic over measured per-entry figures),
same-key racers shed 429 `Retry-After: 1` for the duration of *their* entry's rebuild.

Read against decision 0044's D1 verbatim — a client pays "*only a small penalty (< 0.2 ms), none,
or we need to ensure that longer (multisecond) delays are much rarer than the flush/merge rate*"
— the floor **qualifies**: a fold is floored at one per `compaction_min_interval_secs` (86,400 as
drafted) against a 90 s flush tick, three orders rarer. The pre-swap warm is therefore an
optimisation worth having (it collapses the window to the swap), not the condition on which
"without compromising uptime" hangs. That said, the repair looks tractable: the r3 mechanisms are
all fixable by building the candidate generation's cache *beside* the live one (its own byte
budget, its own keys) rather than warming *into* the live cache, and by keeping `refresh_in_flight`
armed across the swap. That is a disposition for `compaction.md` r4, not this memo.

### 3.4 An option the verified findings open: the two-mode fold

Verified this session (all four composition routes in `compose.rs`, and `visible_to`): **removing
an entity's postings is sufficient invisibility; removing its row is only reclamation.** A row
absent from `base`, `plus` and every post-fold fragment is unreachable by any count, selection,
drill-down or label path; what dead rows cost is range span (inside C4's already-accepted shape),
disc and residency — never an answer.

That licenses splitting the fold's trigger conditions across two modes of one mechanism:

- **Mode A — retirement fold (frequent, cheap for viewers).** Passes 2, 3 and 5 only: postings
  folded (tiers in, tombstones out), external-id runs folded (executed keys dropped, locator
  rescattered), manifests written, prefix flipped, identity rotated, Rule F retirement in the
  swap. **Rows are not touched**, so `segments_version` need not move: every resident projection
  stays exact up to the retired rows, and the repair is `projection − rows_of(retired)` — one
  `andnot` against a set the publication already holds, ~40 ms per resident entry (measured
  class: the ladder's clone figure), not 4 550 ms. Fragments likewise: `fragment − retired` is
  exact, or the ordinary ~200 ms rebuild under the rotated identity. Viewer cost collapses to a
  merge's class.
- **Mode B — reclamation fold (rare).** The full five passes as designed: rows dropped, disc
  reclaimed, dead-row fraction reset — and the projection-rebuild bill of §3.2 paid, at the
  floor cadence.

The trigger gauges split naturally: overlay depth and manifest-bytes pressure dispatch Mode A;
dead-bytes ratio and tombstoned-row fraction dispatch Mode B.

**Two costs, stated so the option is priced rather than admired.** First, Mode A is *not* free of
pass 3: the ingest duplicate check exempts **deleted** holders by consulting the overlay
(verified — `write.rs`'s `established_collisions` filters on `overlay.is_deleted`), so retiring a
deletion without dropping its external-id binding resurrects the dead holder into a 409 against a
legitimate re-ingest. Either the runs fold (streaming, but the coalesced run 0 is the bundle's
largest family — 14.9+ GB at 10⁹ with short keys) or a fourth store records the folded-away set —
and a fourth store is the `deleted` bitmap under another name, which forfeits most of the depth
win and walks back toward the deleted stamp ledger. Mode A therefore pays the postings sweep
*and* the external-id family rewrite; what it saves is row space (~22 GB of columns + morton +
permutation at 10⁹) and, above all, the per-session projection rebuild. Second, it is a second
publication shape through the same seam — more surface on exactly the path r3 already found four
gaps in.

**Verdict:** genuinely attractive for deletion-heavy deployments (bulk revocation, GDPR-class
erasure), where overlay depth crosses its gauge far more often than dead rows do; premature as a
Phase-2 requirement. Record it as the designed answer to "what if `overlay_soft_limit` alarms
weekly but disc is fine", and build Mode B first — Mode A is a strict subset of its passes plus
one publication variant, so nothing is foreclosed by sequencing it second.

### 3.5 Negative results for Q3, so they are not rediscovered

- **A cheaper row-space transform than the full projection rebuild at a Mode-B fold does not
  exist** — attacked in the r3 review and survived: pass 1 globally re-sorts, so rank arithmetic
  cannot patch a projection across it.
- **Merge must not grow into compaction** by consuming the base — excluded structurally and by
  startup relation, for reasons that survive re-examination (write-path §7).
- **Early retirement by staleness or stamps** — deleted from the spec (write-path §5.4); Mode A
  does not re-open it (retirement still happens only at a fold's own publication).
- **A timer-driven fold** — declined in `compaction.md` §9 for reasons this memo endorses: it
  schedules the most expensive operation against a bundle that may have nothing to reclaim.

---

## 4. The index-ordinal split, re-priced against the owner's objection

The sketch (`deferred-index-ordinal-split.md`) prices itself at **two** `u32` arrays — 8 GB at
10⁹ — and leaves its overlay safety argument unresolved. Both halves of that are now stale, and
the corrected shape is materially cheaper. This section is the re-pricing; it does **not**
recommend building ι now.

### 4.1 What changed under the sketch since it was written

- **The overlay objection is dissolved by machinery that exists.** The sketch's central hole was
  that an identity-keyed overlay against ι-keyed postings "mixes spaces on every request".
  Since 2026-08-04 the deny term is not a per-request walk: `Generation::denied` is derived
  **once per geometry publication** from the identity-keyed stores via `row_of`, and composition
  subtracts it as one self-clamping `andnot` (verified, `compose.rs::derive_denied` and the
  module doc). The overlay stays identity-keyed — exactly what Rule S/Rule F safety requires —
  and never needs an ι translation at all, because its only crossing is into **row** space,
  through the permutation that already exists. The WAL likewise stays identity-keyed and replay
  untouched. The sketch's first and worst hole is closed by the current architecture, not by any
  new argument.
- **One of the sketch's two arrays no longer has a consumer.** ι→identity was priced "so the
  gather can still emit a stable `tessera_id`" — written before contracts r6. The gather reads
  `tessera_id` from `columns.arrow` at the row (I10's structural half; verified). No request-path
  consumer of ι→identity remains.
- **identity→ι also has no request-path consumer** in the corrected shape below: ingest allocates
  ι alongside identity (append in lockstep between folds — no lookup); the overlay crosses via
  identity→row (kept); external-ID resolution is `external_id → entity → row` (kept). Drill-down
  is the one candidate, and it has a cleaner route: write-path §14's obligation 27 already pins
  `visible_to(e) ≡ effective.contains_row(row_of(e))` wherever a row exists, so the one bit can
  be answered in row space through the *kept* identity→row array — with the sentinel
  (`ROW_ABSENT` → a probe of a row no mask ever contains) preserving C4's identical-work closure,
  and with the session's projection guaranteed present because 0044 puts full builds at session
  establishment.

### 4.2 The corrected shape and its price

Postings, delta tiers and fragments move to ι-space (dense, renumbered only at a Mode-B fold, in
global signature order); memberships and generating sets follow when Phase 3 builds them. Two
row-directed arrays exist: **`permutation.bin` stays identity→row, unchanged**, serving the deny
mask, drill-down and external-ID resolution; **`iota_to_row.u32` is the one new artefact**,
serving fragment projection. Between folds both extend in lockstep at flush; at a Mode-B fold
both are rewritten (the fold rewrites `permutation.bin` anyway).

Price at 10⁹, against the sketch's 8 GB:

| Item | Cost | Class |
|---|---|---|
| `iota_to_row.u32` | **+4 GB** disc/resident (one `u32` × live entities, dense — *smaller* than the identity-indexed arrays, which pay holes) | arithmetic |
| remap pass at the fold | pass 2 gains a per-member identity→ι lookup (monotone reads against a mapped 4 GB scratch array — posting members ascend, so access is sequential) and a per-term re-sort (bounded by the largest term: 2.5–5×10⁸ members, 1–2 GB sort transient, spoolable) | modelled — needs its own probe before the memory rule is claimed |
| ι-space tiers at flush | allocator hands out ι beside identity; tiers keyed by ι; symmetric with today | design cost only |
| a second I11-class coherence obligation | ι-space artefacts (postings, tiers, `iota_to_row`) version together — discharged by the same manifest digest that already keys fragments | design cost only |

**The 8 GB objection is answered by construction: the corrected shape needs one array, ~4 GB at
10⁹, and it is the information-theoretic floor** — ι order (signature) and row order (Morton) are
unrelated, so ι→row is maximum-entropy at ~30 bits/entry (~3.75 GB); no encoding shrinks it
meaningfully. Two shrink attempts examined and rejected: encoding identity→ι as piecewise-monotone
over signature groups saves at most ~40% at real complexity and buys an array nothing needs; and
succinct-permutation cycle-walking (store one direction + sampled shortcuts) solves the wrong
problem here — it trades space for both directions of *one* permutation, and the corrected shape
never needs the second direction of either.

### 4.3 What ι buys, and when

It closes the one quality axis §3.1 leaves open, permanently and invisibly: at each Mode-B fold
the base postings come out globally signature-contiguous, so authorise cost and posting bytes
converge on the contiguous figures (measured at equal coverage: 21.7 ms against 2 885 ms for a
25% head principal; 8.9–36.7× on bytes — both ceilings, both policy-dependent, 1.0× for
`surnames`). It also relieves §16's exhaustion entry from one side: ι-space stays dense, so
postings and fragments stop paying the consumed-versus-live gap (the identity-indexed arrays
still pay it, at 4 B/hole).

It remains correctly **deferred**. Today's measured authorise worst case (588 ms at 10⁹) is
inside Appendix A's stated band; the win is policy-dependent and can be zero; and the sketch's
own trigger — a deployment at 10⁸+ whose `/control/status` fragmentation gauge shows the
un-banked fraction costing authorise budget — is the right gate. What this section changes is the
*entry price when triggered*: one array, one fold-pass extension, no overlay surgery, no WAL
change — roughly half the sketch's stated memory and none of its unresolved safety argument.
I8/I9 still need the representation-only renumbering argument made explicitly at design time, and
the fold's remap probe must run before the O(1)-memory claim extends to it.

**Sequencing consequence:** ι's execution site is the Mode-B fold (the only moment every posting
byte already flows past a writer). Building the fold first is therefore also the enabling move
for ι — a second reason it tops the ranking in §6.

---

## 5. Q4 — what would make `tessera_id` truly stable

Today the identifier is stable across rebuilds, flushes, merges and folds (the entity axis is
untouched — `compaction.md` §6 says so explicitly), and unstable across exactly three events.
Examined in turn:

- **S1 — key rotation** (decision 0025). Irreducible without breaking a ruling. Three routes
  were examined and all fail: *(a)* persist an old→new translation or accept dual-key lookup —
  re-opens C20 verbatim (a caller comparing answers across idsets learns how the mapping moved;
  the register closed this precisely because "no such parameter exists"), and violates 0005's
  nothing-persisted construction; *(b)* never rotate — achievable today by policy (no rotation
  machinery is even built; the token/idset binding is ⊘), but *promising* it deletes the only
  recovery 0014 leaves against a cracked blinding permutation — the mixer is non-cryptographic
  against a near-degenerate plaintext prior, so rotation is the mitigation of record, and a
  deployment that pins the key forever is accepting unrecoverable viewer-plane linkability if the
  key is ever derived; *(c)* make the construction cryptographic so rotation is never needed —
  rejected by 0014's own reasoning (changing a permanent identity-bearing construction under I9),
  and rotation exists for key *compromise*, which no cipher strength removes.
- **S2 — repartitioning** (§10.6: the idset advances at §12.5's prefix flip). This looks
  **removable for free**, and is the one concrete stability improvement available: §16 r21
  records that entity IDs are globally unique across partitions under a single allocator, so an
  item moving between partitions keeps its entity ID and therefore its `tessera_id`; the idset
  advance appears to be a holdover from when a repartition implied reassignment. Worth an owner
  ruling when partitions become real (they are ⊘ today, so this costs nothing now and forecloses
  nothing).
- **S3 — the escape-hatch identity rebuild** (reassign entity IDs in global signature order to
  recover contiguity — the sketch's documented alternative). **ι removes the reason this event
  exists.** That is the deep connection between Q3, Q4 and §4: the only pressure to renumber
  identity is index quality, and the ι split relocates index quality onto a renumberable axis.

Also examined and rejected: deriving `tessera_id` from `external_id` (no bijection without a
table; the table is the dead handle table decision 0032 deleted; and items without external IDs
have nothing to derive from — contracts §2.4's "one identity, supplied or derived"); and a random
128-bit minted identity (already decided against — 0005, the ~25 GiB hot file).

**Answer:** truly stable, no — and the residual instability is a *ruling*, not a gap: rotation
must break identifiers, because breaking them is what a rotation is for (0025), and C17/C20
bound what a stable identifier may cost in exactly the linkability a rotation exists to cut.
What is worth doing: rule S2 away when partitions land, keep ι as the standing answer to S3, and
keep telling consumers what 0003 already tells them — persist `external_id`, treat `tessera_id`
as stable-until-rotation. That is "as stable as it can safely be", and it is close to the
absolute: a deployment that never rotates never breaks an identifier.

---

## 6. Options, ranked

| # | Option | What it changes | Cost | Invariants touched | To build | Right if |
|---|---|---|---|---|---|---|
| 1 | **Build the fold** (disposition r3; post-swap refresh as the floor, pre-swap warm as an optimisation; Mode A recorded, Mode B built) | discharges Rule F, reclamation, segment/tier/run/dict root-reorganisation; gives `overlay_soft_limit` its lever | modelled minutes–hours wall clock at 10⁹; 2× disc peak; per-fold session-rebuild bill (§3.2), within 0044's third arm at the floor; the seam widening is the principal risk (D1, taken deliberately) | Rule F executed (its three gaps close in the same change); Rule S untouched; I2/I4/I9/I10/I11 per `compaction.md` §11; C4 unchanged | r4 disposition + the engineering r3 named (in-process second-prefix open, mmap permutation writer, generation-carried `bundle_identity`) | deletions, disc growth or segment count ever matter — i.e. always, eventually |
| 2 | **Rule the segment axis** (raise cap modestly + tier-saturation gauge on the fold) | bounds the one unbounded read-path axis | a config default + one gauge; pool transient stated at the knob (4.4–4.9× inputs, measured) | none | trivial | sustained ingest is real |
| 3 | **Re-record ι at §4's price; keep deferred behind its trigger** | preserves the option at half its recorded cost; makes the fold its execution site | one memo edit now; +4 GB and a fold-pass extension *when triggered* | I8/I9 need the representation-only argument at design time; I10/C4 routes stated in §4.1 | plan review when the fragmentation gauge fires | the un-banked authorise fraction ever costs real budget at 10⁸+ |
| 4 | **Decline absolute `tessera_id` stability; rule S2 away when partitions land** | closes the question; one future ruling | none now | 0005/0014/0025/C17/C20 all left standing | a paragraph, later | — |

**Recommendation: 1, then 2, with 3 and 4 as recorded positions rather than work.**

**What it costs if the recommendation is wrong.** If the fold's seam widening (D1's accepted
risk) proves harder than r3 suggests, the fallback the corpus already names is the offline fold
(publish, then restart) — an availability window and the 40–53 s dictionary rebuild, ugly but
safe, and the work done on passes 1–5 carries over unchanged. If deferring ι is wrong — i.e. a
real deployment hits the authorise wall before any fold exists — the un-banked cost is bounded
and measured (588 ms worst case at 10⁹ today; 2 885 ms for a scattered head principal), it
degrades authorise latency and fragment bytes only, never the render path and never correctness,
and the escape hatch (identity-breaking rebuild, consumers re-resolve via `external_id`) remains
available at exactly the price §10.6 and C17 already document. If the two-mode fold is never
built and a deployment turns out deletion-heavy, the floor is Mode B run at Mode-A cadence:
correct, within 0044's letter, and paying the projection-rebuild bill more often than it needed
to — a cost knob, not a hazard.

## 7. What must be measured before more is claimed

Named probes, in addition to `compaction.md`'s P1–P3 (fold RSS/wall-clock, flip cost with a
populated cache, page-cache pollution):

- **P4 — the ι remap pass**: per-member identity→ι lookup + per-term re-sort over the measured
  posting distribution at 10⁷, with the memory rule asserted; nothing about §4.2's "modelled"
  row may be believed without it.
- **P5 — `derive_denied` at depth**: the per-publication cost at 10⁵–10⁷ overlay entries (the
  gauge that decides whether Mode A is ever needed; currently arithmetic over the 1.33 µs apply
  figure).
- **P6 — window-scope fragmentation**: `/control/status`'s `fragmentation` gauge trended over a
  sustained-ingest run, which is the ι trigger's own instrument and has never been read at scale.

## Appendix — verifications performed for this memo

The three findings supplied by the commissioning session, re-verified: (1) postings-removal
sufficiency — all mask consumers route through `EffectiveMask` (`count_range` /
`rows_in_range`-family / `contains_row`) or `verdict`+`fragment.contains` (`visible_to`), so an
entity absent from postings and overlay is unreachable in every verb; rows are reached only
through masks (`compose.rs`, `permutation.rs`). (2) `Generation::denied` is derived per
publication (`derive_denied`), subtracted as one self-clamping `andnot`; per-request work is
O(evaluate entries + buffer), not O(denies) (`compose.rs` module doc and body agree with
write-path §5.3). (3) Fragment build flat in tier count and viewport linear in segments — probe
records at `probes/2026-08-04-refresh-ladder/` (199 ms at 1 tier / 198 ms at 512) and the scale
memo §3 (1.4–1.6 µs per tile × segment). Additionally verified: `Allocator::try_new` refuses a
seed at or above `u32::MAX` (`write.rs`); the duplicate check's deleted-holder exemption reads
`overlay.is_deleted` (`write.rs`), which is what §3.4 prices Mode A's pass-3 obligation from; the
Feistel construction and round count against the vectors (`identity.rs`); and the corrected
on-disc ratios (1.32–1.59×) against the 2.0–2.6× band still quoted elsewhere — the
`2026-08-05-write-path-at-scale.md` correction block governs, and its own closing paragraph plus
`compaction.md` §0/§9 still carry the stale band and should be read against it.
