# Phase 0 memo — go / rework / stop

**Date:** 2026-07-27. **Recommendation: GO — scoped.** Go for Phase 1
spend: nothing measured contradicts an invariant-bearing assumption, and
several findings close escape hatches. **The plan's Phase 0 gate itself
stays open**: plan §4 requires real predicates and real grant sets, this
run used synthetic policies over a real corpus, and the real-label rerun
(action 3) **retains full go / rework / stop authority** — it is not
parameter tuning.

Numbers: `results.md` and `dataset.md`; engineering consequences distilled in `optimisations.md`. Environment: WSL2, 12
cores, 39 GB, RTX 3080; timings indicative, not certified. This memo was
independently reviewed against the design documents and the measurement
record; all findings are incorporated (review record in session notes).

## 1. What was measured, and what this rehearsal is

A **rehearsal on a real corpus with synthetic policies**: 2.42M arXiv
papers (metadata × topic embeddings; join verdict go, with the corpus
caveats in §4), and label generators pinning the distributional knobs
real policies would set — realistic skew (categories, surnames),
orthogonal controls (hash), 361,837-term vocabulary, per-item noise.
The suite is the instrument; it reruns unchanged when real labels exist.

Two reframings of plan §4.1, argued in `results.md`:

- **DNF expansion was not measured** — our predicates are our own knob,
  so measuring their expansion is circular. Deep nesting is the term
  generator's problem, resolvable by minting synthetic terms. Minting is
  a conservation law with two ends: the auth-side end
  (satisfied-terms-per-token) was swept directly to w=10⁴, and the
  item-side end was stress-tested at terms/item = 100 (242M pairs;
  results.md §7): the union path is nearly insensitive to redundancy
  (r≈70 costs 1.11 ms — CRoaring unions price per container, and
  overlapping postings share container structure), so item-side
  inflation lands on storage/index (×t), ingest (×t) and the oracle
  path — never the authorise hot path or the retrieve path, which has
  no term axis. **The true terms/item distribution — and therefore the
  cap-adequacy kill criterion — remains the real-label rerun's**, since
  at t̄ ≥ 100 the cap is a policy parameter rather than a gate.
- OPA-style and accumulo-access-style policies collapse at the seam to
  the same exploded pair relation; the styles return as conformance
  fixtures, not measurement axes.

## 2. The three planned measurements

**2.1 Term/pair profile (plan §4.1).** Category-like policies sit far
inside the 64-term cap (max 13 terms/item). The one overflow source is
author-style labels: **0.318%** of items (mega-collaborations, max 2,561
terms/item) — inside the "low single digits" kill line, so default-deny
overflow holds *for the policy shapes tested* (see §1's item-side gap).
Realised vocabularies to 404k terms behave. Pair-table line item
measured: 8 B/pair in memory, ~4.5 B/pair as parquet → ~80 GB raw at
10⁹×10 pairs, as the plan anticipated.

**2.2 Mask characteristics (plan §4.2).** Stated in both formulations,
because the plan's kill criterion names one and the design's serving
shape is the other:

- *The plan's formulation* — semi-join over the pair table — passes at
  the real corpus (36 ms at w=10⁴) but **trips its own criterion at
  scale**: 3.9–10.3 s for w=10⁴ at the tiled 249.5M corpus, tens of
  seconds extrapolated at 10⁹.
- *The design's §6.3 serving shape* — union of per-term postings — is
  4–540× faster (direction uniform; 4–6× in sparse cells, hundreds× in
  dense ones) and stays **36–530 ms at 249.5M**, ~0.14–2 s extrapolated
  at 10⁹ unsharded. The measured path is Python-driven native CRoaring;
  remaining headroom is parallelism across partitions/shards, not
  language.

Consequence, recorded rather than smoothed over: this **reassigns the
semi-join** from the plan's "authorise budget" role to build-cadence
machinery and the differential oracle. And per the plan's own
consequence clause, a ~2 s worst case at 10⁹ unsharded sits in the band
where **the caller's token-refresh cadence must be planned for it, or
sharding brought forward** (scaling analysis: per-shard builds
parallelise flat). At ≤10⁸ nothing binds.

Mask sizes sit exactly on Appendix A's dense-bound arithmetic (31.2 MB
at 250M for ≥25% coverage; the frozen-format sizes specifically are
unmeasured — pyroaring exposes only the portable format — and fold into
the croaring verification task). Probe-to-dictionary ratios are ≤0.03
in all realistic scenarios (the degenerate categories w=100 grant, 0.57,
excepted) — per-term-lookup territory under the 0.12 heuristic.

*Spatial autocorrelation* (the §16 open question): flat-hash control
exactly 1.00, validating the estimator; surnames 1.03–1.15; categories
1.7–2.32; archive head 5.11. **Realistic masks are essentially
scattered under Morton order**, and Morton buys no Roaring compression
for any family. Consequences: candidate lists serve only the dense
cores of head principals — at working coverages 12–99% of occupied
depth-6 tiles sit below the ~5% crossover (98.8% for a tail-only
principal at 0.13% coverage; 63.3% for a single-surname principal at
4.6%; 12.4% at 10% coverage) — so direct evaluation is the main path,
per r14 §7.2, with duty-cycle numbers; and the scaling analysis's
uniform-scatter assumption is measured ≈true, so its residency figures
are forecasts, not floors. The categories-vs-hash gap (≤5.11× vs 1.00×)
quantifies the topic-correlation caveat.

**2.3 Permission-signature histogram.** 54,791 signatures over 2.42M
items for category-like policy (top 500 groups = 82.4% of corpus);
noise erosion is **gradual, not a cliff** (ε=0.3 still averages
~13/group). Author-like policy is near-unique (1.54M) — partitioning
dead there, as expected. Signature-aligned layout
(signature-major, Morton-minor, size-thresholded) is therefore live
upside for category-like policies and implementable as
group-per-segment under the system-architecture doc (r4) with no new
query shapes; knee at K ≈ 250–1,000 aligned groups ⇒ threshold ≈
0.02–0.05% of corpus. **Its recorded costs come with it**: per-tile
segment fan-out at ~6–8× the design's budget (mitigated by
whole-segment skip), a group-aware merge policy, and predicate changes
becoming physical row moves. It remains a per-deployment build decision
made from the real-label histogram; the whole-group visibility shortcut
is invariant-bearing and lands with the conformance suite, not the
walking skeleton.

## 3. Findings that adjust emphasis (not architecture)

1. **Mask memory × concurrency remains the binding 10⁹ constraint**, now
   as measured arithmetic with no compression rescue. Frozen-mmap
   residency and, where the histogram allows, signature-aligned layout
   are the answers already in the documents.
2. **§11.1's posting compression is not free from arrival order** (run
   lengths 1.0–1.26 under created-order IDs) **but pays hard once
   implemented**: simulated signature-sorted assignment compresses
   postings 8.9× (categories) to 36.7× (t=100) and the 25%-coverage
   session fragment 6× to 1,614× — figures that grow with corpus scale,
   since group runs lengthen while signature vocabulary is
   policy-bound. This attacks the same mask-memory ceiling as finding 1
   from the entity side, at zero query-path cost (row space untouched).
   Surnames-like policies get nothing; the histogram decides, again.
3. **I7's exact fallback is load-bearing**: for tail-only principals
   essentially every tile is below the crossover; the Phase 2 warning
   against "simplifying" it away is not hypothetical.

## 4. Caveats

Synthetic policies bracket, and do not measure, real-label behaviour —
the gate in the first paragraph exists because of this. Corpus
representativeness: embeddings coverage ends 2024-09 and a background
3–6%/year of snapshot papers lack embeddings (join-probe findings), so
the corpus is the joined set, not "arXiv". One UMAP instance (hashed
artifact `4a59a9b8…`); WSL2 timings; retrieve-path latency deliberately
unmeasured (Phase 1's exit criterion, on certified hardware). The 103×
tiling is valid for build cost/size only and holds vocabulary fixed —
for author-like policies a real 250M corpus would have a larger surname
vocabulary and smaller per-term postings, so the measured union costs
overstate that case (the error is conservative).

## 5. Actions

1. Proceed to Phase 1 per the implementation plan as amended by the
   system-architecture doc (now r4: Rust-native `tessera build`
   dissolves the dual-tiler obligation; session-plane split; Appendix R
   actions against the plan), holding two cheap seams open: tile → *set
   of ranges* (§11.3 forces it anyway) and row-order as a build
   strategy parameter.
2. ~~Before Phase 1 commits: verify croaring frozen views, maturin,
   PyPI name.~~ **Done — `pre-phase1-verifications.md`.** No blockers:
   frozen serialize + view both bound in `croaring` 2.7.0 (32-byte
   alignment becomes a bundle-format requirement); maturin fine; the
   PyPI name `tessera` is taken by an abandoned 2017 package, so a
   distribution-name fallback must be chosen at Phase 1 start. Two
   minor gaps recorded there: no `range_uint32_array` binding
   (workaround available), and `Bitmap64` has no frozen support —
   an argument for keeping cached fragments 32-bit.
3. **When real labels exist, rerun the suite — profiles, signature
   histogram, autocorrelation — as Phase 0 proper, with full
   go / rework / stop authority.** DNF expansion and the item-side term
   cap are measured then, on the predicates that actually exist; K for
   any signature-aligned layout is set from that histogram, not this
   one.
4. **Re-examine what the per-item cap is for.** A cap bounds the
   *symptom* of DNF blowup, not the cause: as an early-abort threshold
   during normalisation it genuinely bounds ingest memory and time
   (Fontoura's depth-3 RAM exhaustion), but it cannot make a deeply
   nested predicate work — it declines it, and the decline makes the
   item invisible to every principal, including ones who plainly
   satisfy it (§16 leaves that unresolved). Minting synthetic terms for
   subexpressions removes the expansion instead of capping it, keeping
   terms-per-item linear in expression size at any nesting depth; the
   core is indifferent because terms are opaque, and Appendix E already
   relies on the same idea one level down ("clause widths do not
   contribute, since units are atomic"). Both sides of that trade are
   now measured and cheap — auth-side union width to w=10⁴ (36-530 ms
   at 250M) and item-side redundancy (r≈70 costs 1.11 ms) — with the
   cost landing on dictionary scale, which the surnames config exercises
   at 117M terms. **Consequence for the real-label rerun:** plan §4.1's
   kill criterion ("more than a low single-digit percentage of items
   overflow the cap") measures the naive normaliser's failure rate, not
   the system's. Under minting it is close to vacuous, and the questions
   that matter become declared dictionary scale and union width.

   **Proposed design change, pending the `hiterms` numbers: drop
   exclusion as the overflow response.** Checked against the invariants,
   nothing in principle prohibits an item carrying thousands of terms —
   I5's permission-homogeneity is a per-term property, I2/I3/I7/I13 do
   not reference terms-per-item, and the retrieve path has no term axis.
   The decisive argument is the failure mode's *direction*: a predicate
   is a disjunction, so more terms means broader intended visibility, and
   the cap answers "visible to many" with "visible to none" — a resource
   guard producing an authorisation-shaped outcome, and the exact inverse
   of policy intent. Default-deny is correct when visibility cannot be
   determined; it is not correct when it can be and the answer is merely
   large.

   Keep two smaller things in place of the gate: (a) a **declared bound
   as a sizing hint**, which is §6.1's actual purpose — the service
   cannot size index, union or ingest buffers without one; and (b) a
   **runaway guard at an absurd threshold** (10⁵–10⁶) that warns and
   alarms rather than excluding, so a single pathological predicate
   cannot stall ingest. That distinguishes pathological from
   legitimately broad, which a cap of 64 cannot.

   **This is not an argument for high-t data being normal, and the
   term-based optimisations stay.** High terms-per-item is expected to
   be rare; §7.9's per-term tile histograms and signature-aligned
   partitioning remain valuable for the ordinary case and are already
   *conditional* by design. What changes is only what happens to a
   deployment whose data is pathological: today it gets silently
   invisible items, and under this proposal it gets a warning and worse
   performance. The principle: **performance degrades with data shape;
   availability does not.** The cap inverted that — protecting
   performance at the cost of availability, at a threshold that cannot
   distinguish a pathological predicate from a legitimately broad one.

   **What `hiterms` must answer before this is adopted**: index and
   pair-relation size at 10–1000 terms/item; mask build and union width
   under that breadth; and — the one assumption high t genuinely
   stresses — §7.6's unmeasured "a typical node draws on a modest number
   of terms", which governs how many candidate generating sets a
   labeller faces and how long the fallback ladder runs. Containment
   stays exact either way; what is at risk is label *availability* and
   the caller's labelling cost. This amendment touches §6.1, §6.2 and
   §16 and should go to independent review before the design is revised.
5. **Fix a phantom constant across the companion documents.** The
   implementation plan §4.1 and the prior-art synthesis both cite
   "§6.2's sixty-four-term cap", but the design specifies no number:
   §6.1 makes the per-item cap a *declared bound* supplied by the
   authorisation plugin, and §6.2 says only "the declared per-item term
   cap". 64 is therefore a working assumption, not a spec constant.
   Either the design should fix a default and say so, or the companions
   should stop attributing one to it. Everything measured here reports
   the terms-per-item *distribution*; "% over 64" is a comparability
   readout, not a verdict against a specification.
6. Design-doc bookkeeping at next revision: fold in the architecture
   doc's Appendix R actions against the design (residual-channel
   entries, dictionary address, §2.3 mask-cache key refinement /
   verifiable-auth-data — action 5); note the measured autocorrelation
   bracket against §16 and the §11.1 baseline; record the semi-join
   reassignment against plan §4.2.
