# Independent review — records-and-search design r2

**Date:** 2026-08-12 · **Status:** Review memo — findings for owner disposition; rules nothing ·
**Reviewers:** three independent lenses (security/invariants, performance/evidence,
implementability/coherence), consolidated and every citation re-verified by the orchestrator.
Section references: **records §n** is the design under review; other citations as in its own
convention. §12.1's rulings were not re-litigated; findings against them are unstated
consequences only.

---

**The security argument survives.** No specified mechanism breaks an invariant: the
Truman-consistent scoring rule, positivity of every new operand, C11's layer-scoped term
identity, the sentinel-scan rule, and I7/I9/I12 all held under deliberate attack (the attempted
refutations are listed at the end so they are not re-raised). What the security lens found is
posture, not mechanism: a blob read that is not fail-closed against its one new failure class, an
unstated restriction the 0067 acceptance silently depends on, and phrasing at two new routes that
invites the exact Rule-S elision surface §5.1 exists to forbid.

**The read-side cost argument survives, with corrections; the write-side cost argument does not
yet exist.** Every load-bearing read-side figure reproduces from its probe or is honestly
⊘-marked, and no correction flips a sign or unsettles a §12.2 recommendation — but several
figures need re-marking (probe-only code quoted as measured), re-pairing (a storage comparison
that pairs one column's dictionary with another column's blob), or re-ranging (a claimed 10–40×
whose measured table contains an outright loss). The new coalesce/fold work — the text fold at
10⁹ especially — is priced nowhere, deferred *to this design* by the probe that measured the
storage, and absent from §11's owed measurements.

**The seams do not yet survive.** One collision with built machinery breaks the design as
written for a declaration class the corpus itself uses as its worked example (B1). Three
mechanisms are under-specified at invariant-bearing points — the coalesce's ordinal remap, the
text fold's source, the record blob's addressing — and the owed-amendments enumeration the house
convention requires is missing while at least a dozen normative statements are falsified.

Verdict in one line: **needs-rework before promotion; no finding invalidates the shape, the
rulings already made, or the §12.2 recommendations.**

---

## Blocking findings

**B1 — Store-once deletes the artefacts the built category-vocabulary machinery runs on, and the
design claims that machinery "stands". Breaks-the-design for `render` × category-with-`listing`.**
What the corpus says now: a `per_viewer` category gets a value column *and* postings **whatever
its `used_for` says**, because the listing gate is membership-derived (per-point-attributes §3.3)
and the member sets it needs are the postings derived from that column —
`tessera-engine/src/filter.rs` `owes_value_column` documents exactly this, and `/v1/categories`
is answered from postings plus the value-column extent tail. Placement §2.1 names the limit the
row-space route has: it "cannot answer `/v1/categories`' membership question, which is
entity-space and is what `listing = "per_viewer"` is owed" (placement memo, verified). What the
design says: records §6.2 stores **no** entity-space copy for a rendered column "whatever `index`
says", while records §4.2 asserts "Everything in attrs §3 and index §2.3 stands" — and it
inherits two of placement §2.1's three bounds (per-slice; rendered numbers) while silently
dropping this third. Consequence: a rendered `per_viewer` category — the corpus's own worked
example, attrs §4.2's `department`, and two engine test fixtures — has no member sets and no
extent tail; `/v1/categories` refuses or silently omits post-build values. A **public** rendered
category loses the postings 0063's route split and C24's figures presuppose. And a
**blob-resident** category (`index = false, render = false`) still requires `listing` and
`vocabulary` at parse (records §2) with no stated meaning for either. Options: (i) exempt
categories carrying vocabulary controls from store-once (they keep the entity column — smallest
change, costs the second copy only for that class); (ii) derive the member sets and postings from
the hot column via `row-entity.u32` plus a specified flush-tail mechanism (keeps store-once, adds
a mechanism and a proof that the two halves agree — the exact bug index §5's marker records);
(iii) refuse `render = true` on a category with `listing = "per_viewer"` (narrows the surface;
breaks the worked example). Recommendation: (i) — the second copy for categories is bounded by
the code width, and the exemption is one sentence beside the store-once rule; whichever is
chosen, the design must also say what `listing` means for a blob-resident category.

**B2 — "The same shape as the authorisation side's dictionary-extent merge" is false in the
load-bearing respect, and the shipped coalesce guard does not cover what the keyword merge can
get wrong. Must-fix-before-normative.** What the corpus says now: the authz dictionary-extent
merge is **ordinal-preserving by construction, "which is the whole of its correctness argument"**
(`tessera-authz/src/dict.rs`, verified) — CLAUDE.md lists "dictionary extents positional" among
the format-stability rules that stay. The shipped attribute coalesce's only content guard is
`merge_order`'s union-cardinality-equals-sum plus non-interleaving
(`tessera-filter-write/src/lib.rs`, verified) — presence checks, sufficient today because values
are byte-preserved borrowed slices, so index §5.2's "the set of `(entity, column, value)` triples
… a merge of disjoint extents preserves **exactly**" holds by construction. What the design says:
records §7's coalesce *renumbers every ordinal* through a merged dictionary — the opposite shape
— and inherits "the union-equals-sum overlap guard carries over unchanged" as though that were
still the argument. A remap off-by-one recolours a whole window's values with no symptom the
guard can see. What must change: state the merge's own content guard (the remap is monotone
because both dictionaries are sorted; check `merged_dict[remap[i]] == input_dict[i]` per input,
O(keys)); state the reader rule that an extent's ordinals are only ever resolved against **that
extent's own** dictionary — which requires the manifest's `AttrExtent` (today
`{column, values, presence}`, verified) to name the dict and postings files in the same record so
a layer's files swap atomically; and replace the "same shape" claim with what is actually shared
(the window and publication mechanics, not the correctness argument).

**B3 — The text fold's source is unimplementable as written. Must-fix-before-normative.**
Records §7's fold rebuilds "postings (and any payload sidecar) … **from the folded column**" — a
column the text family does not have (records §4.4: no ordinal column; the values live only in
zstd blocks). The two real candidates differ in kind and by orders of magnitude: re-analyse the
folded blob (full blob decompression plus ~10⁹ ICU segmentations per fold — nowhere near the
fold's priced shape, see B4), or merge the surviving layers' dictionaries and postings and
subtract the blanked entities — cheap, but postings-derived-from-postings, the construction the
derived-from-the-artefact-of-record rule exists to avoid, so it needs its own
equivalence-to-a-fresh-build argument. Options as stated; recommendation: the postings merge,
with the equivalence argument written down (the blanked set is exact, the layers are disjoint by
I9, and the conformance differential "folded against layered" in records §10 is the check that
makes the argument testable). Whichever is chosen, records §7 must name it.

**B4 — The new write-side work at 10⁹ is priced nowhere and is absent from §11 — a load-bearing
quantity neither measured, modelled, nor marked. Must-fix-before-normative.** The probe that
settled keyword storage explicitly defers the price to the design: the coalesce cost "is priced
in the design from the authorisation dictionary's measured merge"
(`probes/2026-08-12-keyword-and-list-storage/results.md`, verified) — and no such price appears
in records §7, while `probes/2026-08-03-dict-fst/` measured FST *build*, not a merge. The only
fold price in the corpus is index §6.2's "~12 GB of streaming IO per column, *modelled*" — for a
u32 column. A text fold additionally rewrites the whole blob (~31 GB compressed through zstd
re-compression), rebuilds the token dictionary whole, and rebuilds postings over ~10¹⁰ entries,
inside the nightly gated window decisions 0056/0057 schedule. What must change: a modelled price
for the keyword and text coalesce and fold at 10⁹ (from measured zstd and merge throughput),
marked per decision 0013, and a write-side item added to §11. This does not need to block the
rulings — its sign is not in doubt — but a promoted design whose largest new *recurring* cost is
unpriced fails the corpus's own evidence standard.

**B5 — The record blob's entity→row addressing is unspecified, its extents have no manifest
home, and its stated storage total omits its own addressing. Must-fix-before-normative.**
Records §3 names "a `u32` within-block offset per entity that has a row" but not the structure
that finds an entity's slot, and the two adjacent flourishes forbid both obvious answers: a dense
per-entity offsets array contradicts "an entity with no blob-resident field occupies nothing"
(and costs ~4 GB at 10⁹); a compacted array needs rank over a has-row set — precisely the
presence bitmap "blob fields need no presence bitmaps" appears to rule out (it reads as being
about fields, but an implementer must guess). The like-for-like storage claim moves ~+7%: the
memo's 54.6 GB is 23.6 (index) + 31.0 = (83.6 − 8)/2.44 — compressed value bytes with **no**
addressing — while the flat side's 83.6 includes its 8 B/entity offsets (verified,
record-and-searchability memo §3). A row larger than a 256 KiB block (a long abstract; a multi
text field) fits neither "oversized block" nor "spanning" under the stated
`(compressed offset, first entity)` directory — unspecified. And blob extents cannot live in
`attr_extents`, which is keyed by `column` (verified): a new manifest list, its digest entries,
and a coalesce-policy axis are all needed and none is named; "absence refuses" is stated only on
the extents line, not for the base `blocks.bin`/`directory.arrow`. What must change: specify the
has-row addressing (a presence-style bitmap plus rank is the honest answer; scope the
"no presence bitmaps" sentence to fields), the oversize-row rule, the blob's manifest and digest
home with its open-time refusals, and the corrected ~59 GB total (the 1.4× win stands).

**B6 — The blob is the one home whose addressing is non-positional, and a valid-but-wrong offset
returns a *different* entity's record — with no fail-closed check. Must-fix-before-normative.**
The other two homes are positional ("one array read"; "one array index" — records §3), so there
is no offset to get wrong; the blob's indirection is new, and a build or fold bug the digest
cannot catch (digests cover bytes, not addressing consistency) makes drill-down on a visible
entity return a neighbour's row — and blob blocks hold entities the principal cannot see, so the
failure crosses the trust boundary. The corpus treats exactly this class as a fail-closed
obligation: index §2.5/§6.2's "never 'those entities carry no value', the wrong answer in a right
answer's clothes". Records §7's "every file digested; absence refuses" does not reach it, and
records §10's adversarial catalogue has CSR-run-boundary entries but no blob-block-boundary ones.
What must change: bound-check offset and length against the block at read time; carry an entity
discriminant in or beside the row so a mis-address refuses rather than answers; add first/last-
entity-in-a-block entries to the §10 catalogue.

**B7 — The conformance-relation claim contradicts the shipped oracle's documented construction,
and the blob-row format is not specified tightly enough for any independent reader.
Must-fix-before-normative.** Records §3 says the oracle's relation "is read from the artefact of
record itself … never from a parallel emission (index §9's argument, inherited unchanged)" and
records §10 adds "including decoding blob rows". The shipped oracle deliberately does the
opposite and documents why that is *stronger*: `reference/oracle/filters.py` "never opens that
artefact. Its attribute values come from the **fixture's own generation functions** … a build
that wrote the wrong value into `attrs/` and then served consistently by its own wrong bytes
disagrees with this oracle rather than being agreed with … taking the fixture's inputs instead is
strictly stronger" (verified). "Inherited unchanged" is therefore wrong about both the practice
and the argument's direction. Relatedly, §4.4's "one implementation, two accesses" cites "the
`tessera_id` construction's precedent" — but `reference/oracle/identity.py` is an *independent
re-derivation* pinned by shared vectors (two implementations); the subprocess half's actual
precedent is `harness.py` driving the CLI. Options: (i) keep the fixture-input relation for
values (drop "decoding blob rows"; the oracle checks the blob only through the served surface) —
preserves the stronger differential; (ii) genuinely read artefacts, which requires a normative
blob-row spec (tag width, per-family encodings, framing, order) and accepts the weaker
differential with an argument. Recommendation: (i), with one narrow artefact-reading check added
where the fixture cannot see — the blob's addressing self-consistency (B5/B6's territory) —
and the analyser precedent restated as golden vectors plus the harness's CLI precedent.

**B8 — The owed-amendments enumeration is missing: records names two amendments and falsifies at
least a dozen normative statements, one inside the very section it amends.
Must-fix-before-normative.** Records §13 seeks one architecture amendment (§12.2.5's sharpening
of §8.3) plus §9's note on §10.3 — but architecture §8.3's own text paragraph still says
"`utf8` matches stored bytes (`eq`, `prefix`, `contains`)" (verified), and the design deletes
`utf8`. Also falsified but unlisted: §10.3's routing-rule sentence; per-point-attributes' entire
`used_for` surface, refusal list ("`multi = true` at all") and example schema; filter-index §1
("declared `used_for = "filter"`"), §2.6's family table (String/utf8), §5's "no dictionary, so no
promotion and no resolver", §5.2/§6.2's lists-excluded markers; filter-surface §2/§6's operand
and `/v1/meta` tables; contracts §2.2/§2.6/§3.2 (`arrow_type` enumerates `utf8`; the wire family
`"string"`, asserted by `conformance/tests/test_filter_differential.py` — verified). House
precedent is filter-index §6.3's closing "Amendments this design owes elsewhere". What must
change: add the enumeration before promotion. Cheap to fix; expensive to omit — an unlisted
falsified claim is a spec contradiction someone later "fixes" in the wrong direction.

## Non-blocking findings

**N1 — Nothing states that keyword/text per-term postings may serve only whole-value operators,
and 0067's acceptance quietly depends on it.** Decision 0067's bound is existence-plus-coarse-
frequency "of a term the caller must already **possess**" (verified). That premise holds for
`eq`/`in` and fails for `prefix`/`contains`, where the caller supplies a *fragment*: a
postings-accelerated fragment operator would be a timing oracle for guessed fragments — beyond
the ruling. The design routes `prefix`/`contains` to channel-free scans today and §6.4 shows no
budget pressure to move them, so it is safe as written — but records §8's acceptance covers
"keyword postings" with no operator restriction, and a future optimiser closing the ordinal-scan
cell would breach 0067 silently. One sentence at records §4.3/§8, of the positivity-tripwire
kind.

**N2 — Two new routes are phrased against "M_auth" where surface §5.1 requires the composed
verdict, the exact elision that is fail-open for Rule S.** Suppressions touch no attribute
artefact (records §7, correctly), so postings, blob and dictionaries all still contain suppressed
entities; every route over them must compose against the composed verdict (fragment minus
overlay) — surface §5.1: "a scan under a fragment silently resurrects a suppressed entity"
(verified). Records §8 gets counts right but writes the phrase verify's survivors as
"inside `M_auth ∩ matched`", and the row-bounded string route (§6.3) — the per-keystroke,
in-viewport surface where a resurrected suppressed item would actually be seen — never restates
the requirement. State once, bindingly, that every route introduced here takes the composed
verdict as its candidate.

**N3 — The keyword dictionary figures rest on a non-decodable front-coding model, and
"2–4 B/key measured" excludes one of the probe's own three columns.** `keyword.py`'s
`front_coded()` charges one shared-prefix byte plus suffix bytes — no suffix length, no restarts,
no restart offsets (verified) — i.e. unparseable as stored; the specified format ("binary search
over restarts", records §4.3) costs ~1–2 B/key more, moving the `id` column's headline from 2.9×
to ~2.2–2.3×. And `submitter`'s dictionary is 1.7 B/present-entity over 542,489 distinct in 2.4M
= **7.5 B/key** (verified), outside the quoted 2–4 — which is also the figure the FST decline
leans on. The sign survives everywhere (still >2× on every measured shape); restate the bytes as
a floor for the specified format, or re-run with restarts, and scope "2–4 B/key" to the two
identifier columns.

**N4 — The 7–1,269× and 0.48–0.73 ns row-space figures are quoted as measured properties of the
route, dropping the probe's own caveat that they measure probe-only code.**
`filter-placement/results.md` "What this does not settle": "arm 1's R constants **describe the
approach, not code** … Expect the built version to be slower by whatever the segment boundary and
the `ScalarSlice` match cost", plus single-segment/single-slice (verified). The conclusion
survives on headroom (7× at the worst cell); the marking as-is overstates what was measured.
Carry the caveat at records §1/§6.2.

**N5 — The keyword-`contains` dictionary-walk model (0.5–1.5 s at 10⁹ unique keys) uses a
constant the specified structure cannot reach.** The ~6 GB/s rate is index §2.2's *flat-column*
byte-scan (~3.5 ns per ~22-byte value — verified); a front-coded dictionary elides shared
prefixes, so a substring can span an elided prefix and every key must be decoded and searched —
a per-key loop whose floor at 10⁹ keys is ~2–3.5 s before decode copies, above the model's upper
bound. The "÷ cores" escape keeps the budget row honest either way; re-model from a per-key
constant, or fold the measurement into §11 item 4's harness.

**N6 — "Derived postings recover 10–40× wherever a vocabulary exists to key them" misstates
arm 3, which contains an outright loss.** The measured `any_of` table at 10⁸ (verified):
contiguous-1%, mean 2/200 is CSR 8.7 vs postings 10.08 ns/candidate — postings *lose*; the full
ratio range across cells is 0.86×–57×. Ruling §12.2.4's real ground — the surnames re-run, where
postings won every cell — is unaffected. Restate as "up to ~50×, and a slight loss in the one
cheap cell".

**N7 — The three-home honesty note's "6.1 against ~12 B/entity on identifiers" pairs one
column's dictionary with another column's blob, and inverts on the flagship identifier.**
`run-2400000.txt` (verified): `id`'s per-column zstd record is 8.6 B/entity *including the
probe's 8 B i64 offsets* — compressed content ~0.6 B/entity, so under the design's own u32
addressing the blob is ~4.6 B/entity, *cheaper* than DICT+C's 6.1. The ~12 matches
`doi`/`submitter` (~11.7), where the dictionary genuinely wins. Restate per column; it also
softens "`index = true` … is both cheaper and searchable" for near-sequential identifier shapes.

**N8 — The rank-aligned payload sidecar is specified for the fold only, while §7 merges postings
per term at every coalesce — ranks shift and the sidecar's coalesce behaviour is unstated.**
Concatenation per term is in fact valid (the merge refuses interleaving, so a merged posting's
rank sequence is its inputs' concatenation), but nothing says so, and as written coalesced
extents would carry postings whose payloads are invalid until the next fold. State the coalesce
rule and count the sidecar toward the per-column input cap.

**N9 — The analyser and the keyword sort/front-code are placed "at the commit-window close",
which write-path (normative) assigns to flush execution on the pool.** Write-path §2.3's close
is the serial group-commit section shared by both lanes; §4.3 writes extents at flush execution
on the pool (verified). As written, ICU segmentation joins the serial ack path. Either the stage
is misnamed (say "flush execution") or the placement is intended and needs its latency argument
made against write-path §2.3 explicitly.

**N10 — `record` is not a reserved column name, so `attrs/record/` collides with a legal
declaration.** `check_column_name` reserves `tessera_id`, `residual`, `external_id`, `x`, `y`,
`access`, `node_id` and the combinators — `record` passes (verified). Reserve it at parse, or
move the blob out of the column namespace.

## Notes

**X1 —** The block-compressed blob adds a coarse C4-shape timing surface at drill-down: one
block's decompression cost reflects the content of a positional run of entities including
invisible neighbours. Weak and single-interaction, but the register is exhaustive only if new
surfaces are named — register the row or record the derivability argument that closes it. (The
`/v1/items` structural closure in Appendix C's C4 annotation is untouched: invisible items never
reach the blob read.)

**X2 —** When 0067's Appendix C row lands, it should distinguish the two possession bounds: for
keyword the caller must possess a whole identifier; for text any common word qualifies, so the
text arm is a corpus-wide term-frequency oracle over the whole vocabulary. The ruling accepts
this knowingly; the row should not read tighter than the text arm is.

**X3 —** An unstated consequence of the ruled refusals: a number declared
`used_for = ["render", "filter"]` is built and working today (0064's filter half landed
2026-08-11; `tessera-engine/tests/filtering.rs`'s `score` exercises it — verified) and the new
surface refuses `index` on a rendered number until 0064's render half. A shipped capability
regresses loudly; records §2/§6.2 should name it as a consequence of store-once with the render
half as the restoration path, and the migration surface should list the schemas it breaks.

**X4 — Figure notes**, each verified, none design-changing: the "3.5–6×" bigram refutation's
denominator is the positional-payload alternative (61.7/10.8, 500/145), not the "22.6 B/entity
unigram index" the same sentence cites (that ratio is 2.7×); records §8 attributes both C24
figures to "the shipped readers" where C24 says 2.1 ms is probe arm 9 at 10⁹ and 1.26 ms the
shipped readers at 10⁸; the coarse-cell "42–260 ms at 10⁸" is the R-dense span with the measured
worst cell 267.06 ms shaved, and the probe's own better-variant-per-cell recommendation gives
17–82 ms, so the design is pessimistic against its own evidence there; §5's "8–14 ns per
candidate entity contiguous" sweeps a scattered cell (contiguous cells measure 8.0–10.8); and
§6.3's inherited "9–33 ms" derives from a memo line whose `eq` figure omits the ~15 ns/row
lookup its own sentence adds (with it, ~13.5 ms — still comfortably in budget).

## Things the design gets right that a rewriter might undo

- Keyword and text as two families, split by measurement — the postings-plus-record layout is
  measured *worse* than flat on repeat-heavy keywords; do not "unify" them.
- The unresolved-needle sentinel scan (records §4.3) — skipping the scan on a dictionary miss is
  the work-channel per-point-attributes §3.8 forbids, at the first place a reviewer would not
  look.
- Blob-extent merge order — `merge_order`'s non-interleaving guard against I9's monotone
  allocation makes "concatenation, entity-ascending" sound as stated.
- The route rule as a function of declaration and request shape only, restated at §6 — three of
  the routes would be convenient to choose from statistics and must not be.
- `FilterRows::Viewport`, `EffectiveMask::with_filter` and the crossing exist as claimed
  (`compose.rs`, `viewport.rs` — verified); the mixed-kind tree composition rule is coherent.
- Phrase-by-verify over bigram terms — the refutation is measured (vocabulary explosion,
  ~65% singletons at every scale); do not re-derive bigrams.
- The empty-list-is-absence rule, with the empty string staying refused — the asymmetry is
  argued, not accidental.

## Attacks attempted that did not succeed

Recorded so they are not re-raised; none of these bit, and no reviewer finding was rejected on
verification (every citation above was re-checked against its file).

- **I7 / cap selection:** best-first draw under the mark cap is a deterministic selection over
  the visible matched set, not a sample of a precomputed global structure.
- **I2 / Truman-consistent scoring:** every BM25 input is mask-local; no corpus-global statistic
  is reachable; Appendix D's IDF channel is excluded by construction.
- **Cap-selection order as a channel:** mask-local DF is caller-derivable from counts already
  served; a score over visible data reveals nothing invisible.
- **m-of-n counting:** timing is dominated by per-term corpus-wide posting sizes — inside the
  0067 channel; no joint quantity is disclosed.
- **C11:** ordinals and token ids are layer-scoped, rebuilt at fold, never crossing the
  boundary; no identity outlives an artefact.
- **Work-indistinguishability on scans:** the `contains` walk reads every key whatever the
  needle; the probe route's work is f(candidate); a present and an absent prefix scan
  identically.
- **I12 / positivity:** `match`, m-of-n, phrase verify, the normalised threshold and list
  `none_of` are all positive; frontier anchors stay on `M_auth`.
- **Multi-value `none_of` under layering:** I9 puts an entity's whole list in one layer, so
  element loss is all-or-nothing and positivity's sign is preserved.
- **I10 / blob directory:** an index internal consulted at drill-down, never in the wire
  payload; sits with `permutation.bin` under 0065's wording.
- **Three-home visibility:** no home discloses a value another route would mask; every home
  gates at entity visibility uniformly.
- **The 10⁹ text extrapolation:** properly ⊘-marked and promotion-gated, with the 3.5× sign
  margin and the three-scale trend (23.33 → 22.70 → 23.55 B/entity) genuinely flat.
- **The blob's mixed-row compression assumption and the UUID front-coding caveat:** both are
  marked assumed/unmeasured in the design, as the house rule requires.
