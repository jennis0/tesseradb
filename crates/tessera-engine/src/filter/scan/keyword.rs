use std::ops::Range;

use croaring::Bitmap;
use tessera_filter::{CodeSet, Codes, DictError, KeyMatcher, SortedDict, ValueColumn};
use tessera_types::AttrLocalId;

use crate::filter::expr::FilterOperand;
use crate::filter::{Endpoint, Scalar};

/// The ordinal a needle no dictionary resolves is scanned for.
///
/// **No dictionary can mint it**, which is what makes it a reserved value rather than a convenient
/// one: `SortedDictWriter` refuses the `u32::MAX`-th key, so a dictionary's ordinals run
/// `0..key_count` with `key_count ≤ u32::MAX`, and `u32::MAX` is therefore outside every layer's
/// ordinal space at once. A slot cannot hold it, so a scan for it matches nothing — while still
/// walking every entity in the candidate, which is the whole point (see this module's header).
pub(in crate::filter) const NO_SUCH_ORDINAL: u32 = u32::MAX;

/// What one layer's dictionary turns a keyword operand into: a question about ordinals that the
/// fixed-width scan can answer.
///
/// **Total on purpose, and this is the security-bearing shape.** There is deliberately no variant
/// meaning *matches nothing, so do not scan*. A needle the layer does not hold becomes
/// [`NO_SUCH_ORDINAL`] and a prefix no key carries becomes the sentinel *range*. Every arm of
/// [`scan_ordinals`] then runs a scan over the whole candidate, so a dictionary miss costs what a
/// hit costs — the rule records §4.3 states and per-point-attributes §3.8 requires. A future
/// variant that skipped the scan would have to be added here *and* given an arm there, which is
/// where a reader is most likely to see what it is for.
///
/// **`contains` no longer arrives here**, by either route: it ends in
/// [`ValueColumn::scan_ordinal_set`] over a table sized by the dictionary, where an empty table
/// scans identically to a full one and the same rule therefore needs no sentinel to state it. The
/// move was not for tidiness — a sorted list made the scan's per-slot cost `O(log k)` in the number
/// of dictionary keys carrying the substring, which is a corpus-wide quantity and not one the work
/// may depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::filter) enum OrdinalPredicate {
    /// One ordinal — `eq`, resolved.
    Eq(u32),
    /// A list of ordinals — `in`, **one entry per needle the caller named** and nothing else.
    /// `O(log k)` per slot is priced for that *k*; a set whose size is a corpus quantity belongs in
    /// [`ValueColumn::scan_ordinal_set`] instead.
    In(Vec<u32>),
    /// A contiguous ordinal range, **inclusive at both ends** — what a prefix's dictionary range
    /// becomes, sortedness being the reason the dictionary is sorted.
    ///
    /// **Inclusive because the natural spelling of an empty prefix range is the early return
    /// wearing another hat.** `SortedDict::prefix_range` answers "no key carries this" with a
    /// half-open `k..k`, and where the prefix sorts below every key — an ordinary case, a needle
    /// alphabetically before the whole dictionary — that is `0..0`. Carried through as an
    /// exclusive upper bound of 0, the range scan narrows it to −1, finds it unrepresentable in
    /// the column's `u32` and returns *without scanning* (`values.rs`'s `narrow_hi`, whose
    /// `Unsatisfiable` verdict short-circuits by design for bounds a client wrote). So that
    /// spelling would make exactly the prefixes nobody carries the cheap ones. The sentinel range
    /// `NO_SUCH_ORDINAL..=NO_SUCH_ORDINAL` is representable, matches no slot, and scans.
    Range { lo: u32, hi: u32 },
}

/// A dictionary's half-open prefix range as an inclusive ordinal predicate.
fn ordinal_range(range: Range<u32>) -> OrdinalPredicate {
    if range.start >= range.end {
        return OrdinalPredicate::Range {
            lo: NO_SUCH_ORDINAL,
            hi: NO_SUCH_ORDINAL,
        };
    }
    OrdinalPredicate::Range {
        lo: range.start,
        hi: range.end - 1,
    }
}

/// Resolve one operand against **one layer's** dictionary.
///
/// A miss is ordinary and is not an error: `SortedDict::resolve` says so, and the whole point of
/// [`NO_SUCH_ORDINAL`] is that the scan proceeds. Only a malformed dictionary refuses.
///
/// `in` yields exactly one ordinal per needle the caller named, misses included, so the list this
/// hands the scan has the caller's own length rather than a length that counts how many of their
/// needles this layer happens to hold. The set scan then sorts and de-duplicates it, which leaves
/// one residual difference in work — a list of *k* distinct resolved ordinals against a list of one
/// sentinel costs `O(log k)` more per candidate slot, `k` being the operand's own size and never a
/// corpus quantity. Named rather than engineered around: the scan itself, which dominates, runs
/// identically either way, and the same residual is what the category route's unresolved keys have
/// always had.
pub(in crate::filter) fn keyword_ordinals(
    dict: &SortedDict,
    operand: &FilterOperand,
) -> Result<OrdinalPredicate, DictError> {
    Ok(match operand {
        // `match` is the text family's and reaches no keyword layer: the parse gate refuses an
        // operator outside a column's family, so this is the second line of defence and the
        // sentinel — which scans and matches nothing — is the fail-closed reading.
        FilterOperand::Match { .. } | FilterOperand::Phrase { .. } => {
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL)
        }
        FilterOperand::TextEquals(needle) => {
            OrdinalPredicate::Eq(dict.resolve(needle)?.unwrap_or(NO_SUCH_ORDINAL))
        }
        FilterOperand::TextIn(needles) => {
            let mut ordinals = Vec::with_capacity(needles.len());
            for needle in needles {
                ordinals.push(dict.resolve(needle)?.unwrap_or(NO_SUCH_ORDINAL));
            }
            OrdinalPredicate::In(ordinals)
        }
        FilterOperand::TextPrefix(prefix) => ordinal_range(dict.prefix_range(prefix)?),
        // **The second line of defence, and it still scans.** `contains` is routed by
        // [`keyword_contains`] before this is reached, and the remaining operands belong to other
        // families — the parse refuses each of them on a keyword column, so arriving here means a
        // caller built the expression directly. The sentinel is the fail-closed answer: it matches
        // nothing, and it matches nothing the same way a needle nobody holds does.
        FilterOperand::TextContains(_)
        | FilterOperand::Equals(_)
        | FilterOperand::In(_)
        | FilterOperand::NumEquals(_)
        | FilterOperand::NumIn(_)
        | FilterOperand::Range { .. } => OrdinalPredicate::Eq(NO_SUCH_ORDINAL),
    })
}

/// One ordinal predicate against one layer's `u32` ordinal column.
///
/// **Every arm scans, and none of them may learn to return early.** See [`OrdinalPredicate`].
///
/// The ordinals are handed to the value column as `AttrLocalId` because that is the type its
/// fixed-width scans compare, and the crossing is safe here for a reason worth stating: the value
/// is minted from *this* layer's dictionary two calls above, consumed by *this* layer's column, and
/// never stored, returned or compared against an ordinal from anywhere else. It is a comparand for
/// the width of one call, not an identity.
pub(in crate::filter) fn scan_ordinals(values: &ValueColumn, predicate: &OrdinalPredicate, candidate: &Bitmap) -> Bitmap {
    match predicate {
        OrdinalPredicate::Eq(ordinal) => values.scan_eq(candidate, AttrLocalId::new(*ordinal)),
        OrdinalPredicate::In(ordinals) => {
            let ids: Vec<AttrLocalId> = ordinals.iter().copied().map(AttrLocalId::new).collect();
            values.scan_in(candidate, &ids)
        }
        OrdinalPredicate::Range { lo, hi } => values.scan_range(
            candidate,
            Some(Endpoint {
                value: Scalar::Int(i128::from(*lo)),
                inclusive: true,
            }),
            Some(Endpoint {
                value: Scalar::Int(i128::from(*hi)),
                inclusive: true,
            }),
        ),
    }
}

/// One operand against one keyword layer: resolve in that layer's dictionary, then scan its
/// ordinals.
pub(in crate::filter) fn scan_keyword(
    values: &ValueColumn,
    dict: &SortedDict,
    operand: &FilterOperand,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    if let FilterOperand::TextContains(needle) = operand {
        return keyword_contains(values, dict, needle, candidate);
    }
    let predicate = keyword_ordinals(dict, operand)?;
    Ok(scan_ordinals(values, &predicate, candidate))
}

/// Which of `contains`' two routes a layer takes (records §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::filter) enum ContainsRoute {
    /// Decode and substring-search **every** key in the layer's dictionary, collect the matching
    /// ordinals, then scan for them. Front coding elides shared prefixes, so a substring can span
    /// an elided one and a flat search of the file's bytes would miss matches — it is a per-key
    /// loop, not a byte stream. Reading every key whatever the needle is also what keeps this
    /// route's work a function of `(candidate, column)`.
    Broad,
    /// Take each candidate entity's ordinal and probe the dictionary for that one key. One random
    /// dictionary access per candidate entity, and none at all for keys no visible entity carries.
    Narrow,
}

/// Per-key cost of the broad route's walk, in nanoseconds.
///
/// **Measured**: 11.0–18.8 ns to decode one key, over the three real arXiv columns records §4.3
/// quotes, through the shipped reader at the shipped restart interval
/// (`probes/2026-08-13-keyword-dict/results.md`). 15 is the middle of that band. The figure is
/// decode *alone* — the substring search over the decoded key is not in it — so this understates
/// the broad route and biases the choice towards it, which is the conservative direction: the
/// broad route is the one whose cost is bounded by the artefact rather than by the request.
///
/// The measurement is at 2.4M keys and records §4.3 sizes this family at 10⁹, so cache behaviour at
/// 400× the size is not in it. **The 2–10 s the design originally modelled must not be quoted**;
/// the honest extrapolation is 11–19 s single-threaded at 10⁹ before the search.
const BROAD_KEY_NS: u64 = 15;

/// Per-candidate **upper bound** on the narrow route's cost, in nanoseconds.
///
/// 0.10 µs is one `SortedDict::key_of` at the restart interval the campaign chose — the bottom of
/// records §4.3's *modelled* 0.1–0.3 µs band, and the figure that interval was chosen against
/// (`tessera_filter::DEFAULT_RESTART_INTERVAL`). The substring search over one decoded key and the
/// ordinal scan the broad route also pays per candidate entity (0.25–0.28 ns contiguous) both
/// vanish beside it at this precision, so neither is carried separately.
///
/// **It is a bound and no longer the typical cost, and it is deliberately not lowered.** Since the
/// route walks blocks rather than probing keys, what it pays per candidate entity depends on how
/// that candidate's ordinals cluster: 19.4–59.8 ns measured across six real shapes at a 25%
/// candidate (`2026-08-13-contains-recovery`), against ~100 for the worst case this constant has to
/// cover — a small, scattered candidate over a unique vocabulary, where every entity opens its own
/// block and nothing amortises. Pricing the route at its bound errs towards the broad route, whose
/// cost is capped by the vocabulary where the narrow route's grows without limit in the candidate;
/// pricing it at its typical cost would pick it in exactly the cell where it is worst.
///
/// ⊘ The price of that safety is real and now measured: on a contiguous 25% candidate over a
/// unique column the rule takes the broad route at 41.1 ms where this one costs 11.6 ms. Closing
/// that gap means a rule that consults the candidate's *distinct* ordinal count — a statistic about
/// what the principal's own data contains — which is the fence's stop-and-report A and an §8.2
/// admissibility question the owner has not ruled on. Not closed here.
const NARROW_PROBE_NS: u64 = 100;

/// Choose a `contains` route from **the candidate's cardinality and the layer's dictionary size,
/// and nothing else** (records §4.3).
///
/// `|candidate| · NARROW_PROBE_NS` against `|dictionary| · BROAD_KEY_NS`: the narrow route costs
/// one probe per candidate entity, the broad route one decode per key plus an ordinal scan the
/// narrow route does not run. Both inputs are admissible under §8.2 — the candidate's cardinality
/// is the principal's own quantity, which the caller can compute for itself and already receives as
/// a request's `visible` count, and a dictionary's key count is a property of the bundle, identical
/// for every principal. Neither reads a statistic about *what* the principal's data contains, which
/// is the class §6 forbids a route rule to consult, and neither depends on the needle: the same
/// request over the same mask takes the same route whether the value exists or not.
pub(in crate::filter) fn contains_route(candidate_entities: u64, dictionary_keys: u64) -> ContainsRoute {
    if candidate_entities.saturating_mul(NARROW_PROBE_NS)
        < dictionary_keys.saturating_mul(BROAD_KEY_NS)
    {
        ContainsRoute::Narrow
    } else {
        ContainsRoute::Broad
    }
}

/// `contains` against one keyword layer, by whichever route [`contains_route`] names.
///
/// **The routes are benched against each other and against the flat `utf8` scan they replaced**,
/// in the one window where both formats existed
/// ([the fence](../../../docs/evidence/memos/2026-08-13-utf8-retirement-fence.md)) and again
/// after both routes were repaired
/// ([the recovery](../../../docs/evidence/memos/2026-08-13-contains-recovery.md)). The
/// crossover's constants above are calibration; either route answers correctly whichever is
/// chosen.
fn keyword_contains(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // Every key contains the empty needle, so the answer is *carries a value in this column* — the
    // whole ordinal range, which the range scan expresses without materialising an ordinal per key.
    // No key is read because no key's content bears on the answer, and the needle's emptiness is
    // the caller's own input rather than anything about the corpus.
    if needle.is_empty() {
        let whole = if dict.is_empty() {
            OrdinalPredicate::Range {
                lo: NO_SUCH_ORDINAL,
                hi: NO_SUCH_ORDINAL,
            }
        } else {
            OrdinalPredicate::Range {
                lo: 0,
                hi: dict.len() - 1,
            }
        };
        return Ok(scan_ordinals(values, &whole, candidate));
    }
    match contains_route(candidate.cardinality(), u64::from(dict.len())) {
        ContainsRoute::Broad => contains_broad(values, dict, needle, candidate),
        ContainsRoute::Narrow => contains_narrow(values, dict, needle, candidate),
    }
}

/// The broad route: walk the dictionary, keep the ordinals whose keys contain the needle, scan.
fn contains_broad(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // `SortedDict::walk` has no early exit by construction, so the walk's cost is the dictionary's
    // size and never the needle's selectivity. The matcher is built once outside it for the reason
    // [`KeyMatcher`] gives — a searcher constructed per key was measured at up to 47% of this
    // route — and hoisting it changes no work the walk does per key.
    //
    // Matches go straight into a table over the dictionary's ordinals rather than into a list.
    // Two reasons, and the second is the load-bearing one. It bounds the allocation: a one-byte
    // needle over a near-unique vocabulary matches most of it, and a `Vec<u32>` of that is four
    // bytes per matching key where the table is an eighth of a bit per key whatever matched. And
    // it makes the scan that follows cost the same per slot however many keys matched — see
    // [`ValueColumn::scan_ordinal_set`], which is where the argument lives.
    let mut matched = CodeSet::over_domain(dict.len().saturating_sub(1));
    let matcher = KeyMatcher::new(needle);
    dict.walk(|ordinal, key| {
        if matcher.matches(key) {
            matched.insert(ordinal);
        }
    })?;
    // No key matched: an empty table, and the scan still runs over every candidate slot exactly as
    // it does for a full one. The sentinel `OrdinalPredicate::In` needed to say this is not needed
    // here — an empty domain-sized table already traverses identically — which is the rule stated
    // in the structure rather than in a reserved value.
    Ok(values.scan_ordinal_set(candidate, &matched))
}

/// The narrow route: read only the dictionary the candidate's own values occupy.
///
/// **The route reads the candidate's ordinals, not its entities.** Entities sharing a value name
/// the same ordinal, and `SortedDict::key_of` decodes a whole block prefix to return one key — so
/// probing per candidate entity paid `restart_interval / 2` discarded decodes for every entity,
/// including the duplicates. Deduplicating first and handing the sorted result to
/// `SortedDict::walk_ordinals` pays each *block* once instead. The whole replacement — the
/// deduplication, the block walk and [`KeyMatcher`] — measures **1.91–6.05×** the probe-per-entity
/// loop across six real shapes, best where the candidate is contiguous or its column repeats and
/// worst where it is scattered over a unique vocabulary
/// ([`contains-recovery`](../../../docs/evidence/memos/2026-08-13-contains-recovery.md)).
///
/// **The route now ends where the broad route ends** — one `OrdinalPredicate::In` scan over the
/// candidate — and the two differ only in how the matching ordinal set is computed: from the whole
/// dictionary, or from the blocks the candidate's own values sit in. That is what puts this route
/// inside `tessera_filter::take_scan_work`, which the per-entity probe loop was outside: the
/// keyword shape whose traversal no test could assert is now asserted by the same harness as every
/// other.
///
/// The transient is one `u32` per candidate entity carrying a value. The crossover admits this
/// route only below `0.15 × |dictionary|` candidate entities, so that allocation is at most
/// **0.6 B per dictionary key** against a dictionary the same campaign measures at 4.15–9.61 B/key
/// — a fraction of the artefact it is reading, not a second copy of it.
fn contains_narrow(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // A keyword layer's ordinals are `u32` by construction. Any other width means the column and
    // its declaration disagree, and the fail-closed reading is that nothing matches — the same
    // second line of defence `scan_eq` keeps for a code predicate over a text column.
    let Codes::U32(ordinals) = values.codes() else {
        return Ok(Bitmap::new());
    };
    let held = ordinals.len().min(candidate.cardinality() as usize);
    let mut wanted: Vec<u32> = Vec::with_capacity(held);
    values.for_each_code_in(candidate, |ordinal| wanted.push(ordinal));
    wanted.sort_unstable();
    wanted.dedup();

    let matcher = KeyMatcher::new(needle);
    let mut matched = CodeSet::over_domain(dict.len().saturating_sub(1));
    dict.walk_ordinals(&wanted, |ordinal, key| {
        if matcher.matches(key) {
            matched.insert(ordinal);
        }
    })?;
    // The same table the broad route ends in, for the same reason: an empty one scans every
    // candidate slot exactly as a full one does, so "no key the candidate carries matched" needs
    // no sentinel to say it.
    Ok(values.scan_ordinal_set(candidate, &matched))
}

#[cfg(test)]
mod tests {
    //! The keyword read route, over dictionaries and ordinal columns built in this process.
    //!
    //! **Unit level, and deliberately so for the parts that carry the argument.** A dictionary and
    //! an ordinal column are cheap to build in a test and the interesting cases — two layers that
    //! number one key differently, a scattered presence under a scattered candidate, a needle
    //! nobody holds — are ones a fixture would have to be contrived to produce. What these cover
    //! is every step the route is made of: the per-layer resolve, the sentinel rule, the two
    //! `contains` routes against each other, and the slot arithmetic the narrow one depends on.
    //!
    //! ⊘ An end-to-end pass over a built bundle — a keyword column filtered through a real
    //! principal's mask, which is what `tests/filtering.rs` does for every other family — is owed
    //! and is not here.

    use super::*;
    use crate::filter::test_support::*;
    use crate::filter::FilterColumns;
    use tessera_filter::{take_scan_work, ScanWork};

    // -------------------------------------------------------------------------------------
    // The sentinel rule
    // -------------------------------------------------------------------------------------

    /// **A needle the layer does not hold resolves to the reserved ordinal, never to a shortcut.**
    /// The assertion is on the *representation* rather than on the answer, because both are empty:
    /// what must not drift is that a miss is carried to the scan as a value to look for.
    #[test]
    fn a_dictionary_miss_is_the_reserved_ordinal() {
        let d = dict(&["alpha", "beta", "gamma"]);
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::TextEquals("delta".into())).unwrap(),
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL)
        );
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::TextEquals("beta".into())).unwrap(),
            OrdinalPredicate::Eq(1)
        );
    }

    /// **A prefix no key carries is the sentinel *range*, not an empty one**, and that distinction
    /// is the whole reason [`OrdinalPredicate::Range`] is inclusive — see its doc for the bound
    /// that would make an empty range skip the scan. Both directions are covered: `zeta` sorts
    /// above every key here and `aa` below every one of them, the second being the `0..0` case.
    #[test]
    fn a_prefix_no_key_carries_is_the_sentinel_range() {
        let d = dict(&["alpha", "alpine", "beta"]);
        for absent in ["zeta", "aa"] {
            assert_eq!(
                keyword_ordinals(&d, &FilterOperand::TextPrefix(absent.into())).unwrap(),
                OrdinalPredicate::Range {
                    lo: NO_SUCH_ORDINAL,
                    hi: NO_SUCH_ORDINAL
                },
                "prefix {absent:?}"
            );
        }
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::TextPrefix("alp".into())).unwrap(),
            OrdinalPredicate::Range { lo: 0, hi: 1 }
        );
    }

    /// **The pin on the whole rule: a sentinel predicate is answered by a real scan.**
    ///
    /// The column below holds `NO_SUCH_ORDINAL` in a slot, which no dictionary can mint and no real
    /// keyword column therefore has — so the only way that entity comes back is if the scan walked
    /// the candidate and compared. An implementation that recognised the miss and returned
    /// `Bitmap::new()` — the early return records §4.3 forbids, because it makes *no item has this
    /// value* cheaper than *some do* — returns nothing here and fails, in all three predicate
    /// shapes at once.
    #[test]
    fn a_sentinel_predicate_is_scanned_for_and_not_short_circuited() {
        let column = universal(&[7, NO_SUCH_ORDINAL, 9]);
        let all = set(&[0, 1, 2]);
        for predicate in [
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL),
            OrdinalPredicate::In(vec![NO_SUCH_ORDINAL]),
            OrdinalPredicate::Range {
                lo: NO_SUCH_ORDINAL,
                hi: NO_SUCH_ORDINAL,
            },
        ] {
            assert_eq!(
                members(&scan_ordinals(&column, &predicate, &all)),
                vec![1],
                "{predicate:?} must reach the slot holding the reserved ordinal"
            );
        }
    }

    /// `in` hands the scan one ordinal per needle the caller named, misses included, so the list's
    /// length is the operand's own and never a count of how many of them this layer holds.
    #[test]
    fn an_in_set_carries_one_ordinal_per_needle_hit_or_miss() {
        let d = dict(&["alpha", "beta", "gamma"]);
        let operand = FilterOperand::TextIn(vec!["gamma".into(), "delta".into(), "alpha".into()]);
        assert_eq!(
            keyword_ordinals(&d, &operand).unwrap(),
            OrdinalPredicate::In(vec![2, NO_SUCH_ORDINAL, 0])
        );
    }

    /// An operand from another family cannot reach a keyword column through the parse; arriving
    /// here from an embedder that built the expression directly, it matches nothing **and still
    /// scans** — the same fail-closed answer a needle nobody holds gets.
    #[test]
    fn an_operand_from_another_family_takes_the_sentinel() {
        let d = dict(&["alpha"]);
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::NumEquals(Scalar::Int(3))).unwrap(),
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL)
        );
    }

    // -------------------------------------------------------------------------------------
    // The sentinel rule, asserted in work
    // -------------------------------------------------------------------------------------
    //
    // The tests above pin what a miss *is* — the reserved ordinal, the sentinel range — and what a
    // scan for it *answers*. These pin what it **costs**, which is the property records §4.3 states
    // and per-point-attributes §3.8 requires: a needle no dictionary holds must be indistinguishable
    // from one every dictionary holds in work, not merely in outcome. Records §10 names this as the
    // conformance suite's one deliberate work assertion, "the one place a work assertion is the test,
    // because the rule exists for it".
    //
    // **The unit is traversed slots and runs, never elapsed time.** A stopwatch assertion would be
    // flaky in exactly the direction that lets the channel reopen — it goes green on a loaded
    // machine — so what is compared is the count `tessera_filter::take_scan_work` reports, which is
    // zero for a scan that did not happen and identical for two that ran to completion over the same
    // candidate. That module's header argues where the counter sits and what it costs when tests are
    // not running.
    //
    // Each of these routes through [`FilterColumns::resolve`] rather than through `scan_ordinals`,
    // because the early return this is guarding against has more than one place to hide: the resolve,
    // the predicate, the scan, and — the one no answer-level test can see — the per-layer loop.

    /// The work one operand costs over `column`, and the entities it returns.
    ///
    /// The counter is taken *before* the resolve as well as after, so what comes back is this
    /// resolve's own traversal rather than it plus whatever the assertion before it left behind.
    fn work_of(
        columns: &FilterColumns,
        column: &str,
        operand: &FilterOperand,
        candidate: &Bitmap,
    ) -> (ScanWork, Vec<u32>) {
        let _ = take_scan_work();
        let out = columns
            .resolve(column, operand, candidate)
            .expect("a declared keyword column resolves");
        (take_scan_work(), members(&out))
    }

    /// Every operand in `ops` traverses exactly what the first one does — **and the first traverses
    /// something**, which is what stops the equality holding vacuously if the scan were removed
    /// altogether rather than merely short-circuited for the sentinel.
    fn traverse_alike(
        columns: &FilterColumns,
        column: &str,
        candidate: &Bitmap,
        ops: &[(&str, FilterOperand)],
    ) {
        let (first, rest) = ops.split_first().expect("at least one operand to compare");
        let (baseline, _) = work_of(columns, column, &first.1, candidate);
        assert!(
            baseline.runs > 0 && baseline.slots > 0,
            "{}: the scan traversed nothing, so the comparisons below would hold vacuously. A \
             --release build is the ordinary cause — the counter is compiled under debug_assertions \
             (tessera_filter::take_scan_work)",
            first.0
        );
        for (label, operand) in rest {
            let (work, _) = work_of(columns, column, operand, candidate);
            assert_eq!(
                work, baseline,
                "{label} traversed differently from {}: the two must cost the same",
                first.0
            );
        }
    }

    /// **`eq` costs the same whether or not the needle exists.** The three needles below are a key
    /// the layer holds, a key sorting after every key it holds, and one sorting before all of them —
    /// so a short circuit reached by any of the resolve's paths shows up as a shorter traversal.
    #[test]
    fn eq_traverses_alike_whether_or_not_the_needle_resolves() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 0, 2]),
                dict(&["alpha", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        // The held needle really does match, so the equality below is two scans that found
        // different things, not two that found nothing.
        let (_, held) = work_of(&columns, "sub", &eq("beta"), &candidate);
        assert_eq!(held, vec![1]);
        let (_, absent) = work_of(&columns, "sub", &eq("delta"), &candidate);
        assert!(absent.is_empty(), "no entity carries delta");

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a needle the dictionary holds", eq("beta")),
                ("a needle no dictionary holds", eq("delta")),
                ("a needle sorting below every key", eq("aa")),
            ],
        );
    }

    /// **The shape a flush produces: layers that disagree about a key.** `alpha` is in the base's
    /// dictionary and not the extent's, `gamma` in the extent's and not the base's, `delta` in
    /// neither — and all three must scan both layers in full.
    ///
    /// This is the case a naive early return optimises and no answer-level test can catch: skipping
    /// a layer whose dictionary does not resolve the needle returns exactly the right entities, from
    /// exactly the layers that could hold them, for less work — which is the channel, and is why
    /// these route through [`FilterColumns::resolve`] rather than through one layer's scan.
    #[test]
    fn eq_traverses_alike_where_the_layers_disagree() {
        let columns = keyword_column(
            "sub",
            vec![
                // Base: alpha = 0, beta = 1, over entities 0..3.
                (None, universal(&[0, 1, 0]), dict(&["alpha", "beta"])),
                // Extent: beta = 0, gamma = 1, over entities 10 and 11.
                (
                    Some("extents/f1.arrow"),
                    partial(&[10, 11], &[1, 0]),
                    dict(&["beta", "gamma"]),
                ),
            ],
        );
        let candidate = set(&[0, 1, 2, 10, 11]);
        for (needle, expected) in [
            ("alpha", vec![0, 2]),
            ("gamma", vec![10]),
            ("beta", vec![1, 11]),
            ("delta", vec![]),
        ] {
            let (_, out) = work_of(&columns, "sub", &eq(needle), &candidate);
            assert_eq!(out, expected, "{needle} over both layers");
        }

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a needle both layers hold", eq("beta")),
                ("a needle only the base holds", eq("alpha")),
                ("a needle only the extent holds", eq("gamma")),
                ("a needle neither holds", eq("delta")),
            ],
        );
    }

    /// **A prefix nothing carries costs what a prefix everything carries costs**, including the
    /// prefix that sorts below every key.
    ///
    /// That last one is the case [`OrdinalPredicate::Range`]'s inclusive bound exists for: as a
    /// half-open `0..0` it narrows to an upper bound of −1, which the range scan finds
    /// unrepresentable and answers *without scanning*. `zeta` sorts above every key and `alphabet`
    /// falls inside the dictionary while matching no key, so all three empty shapes are here.
    #[test]
    fn a_prefix_matching_nothing_traverses_what_a_matching_prefix_does() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 0]),
                dict(&["alpha", "alpine", "beta"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let (_, matched) = work_of(&columns, "sub", &prefix("alp"), &candidate);
        assert_eq!(matched, vec![0, 1, 3]);

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a prefix two keys carry", prefix("alp")),
                ("a prefix sorting below every key", prefix("aa")),
                ("a prefix sorting above every key", prefix("zeta")),
                (
                    "a prefix inside the dictionary that no key carries",
                    prefix("alphabet"),
                ),
            ],
        );
    }

    /// **An `in` set costs the same however many of its needles resolve.** The three sets below name
    /// three needles each — the operand's own length held equal, because the per-slot search is
    /// `O(log k)` in *k*, the caller's own quantity, and it is the traversal rather than *k* that
    /// must not vary with what the corpus holds.
    #[test]
    fn an_in_set_traverses_alike_however_many_needles_resolve() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 1]),
                dict(&["alpha", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let (_, some) = work_of(
            &columns,
            "sub",
            &in_set(&["alpha", "delta", "zulu"]),
            &candidate,
        );
        assert_eq!(some, vec![0], "only alpha of the three is held");

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("every needle resolves", in_set(&["alpha", "beta", "gamma"])),
                (
                    "one needle of three resolves",
                    in_set(&["alpha", "delta", "zulu"]),
                ),
                ("no needle resolves", in_set(&["delta", "zulu", "aa"])),
            ],
        );
    }

    /// **`contains` too**, which records §4.3 states the rule for by name: the broad route's walk
    /// reads every key whatever the needle, and the ordinal scan that follows it runs on the
    /// sentinel when no key matched. The candidate and dictionary here put [`contains_route`] on the
    /// broad route, which is the one whose scan this counter sees.
    #[test]
    fn a_contains_matching_no_key_traverses_what_a_matching_one_does() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 1]),
                dict(&["alpha", "alpine", "beta"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        assert_eq!(
            contains_route(candidate.cardinality(), 3),
            ContainsRoute::Broad
        );
        let (_, matched) = work_of(&columns, "sub", &contains("lph"), &candidate);
        assert_eq!(matched, vec![0]);

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a fragment one key contains", contains("lph")),
                ("a fragment no key contains", contains("zzz")),
            ],
        );
    }

    fn eq(needle: &str) -> FilterOperand {
        FilterOperand::TextEquals(needle.into())
    }

    fn prefix(needle: &str) -> FilterOperand {
        FilterOperand::TextPrefix(needle.into())
    }

    fn contains(needle: &str) -> FilterOperand {
        FilterOperand::TextContains(needle.into())
    }

    fn in_set(needles: &[&str]) -> FilterOperand {
        FilterOperand::TextIn(needles.iter().map(|n| (*n).into()).collect())
    }

    // -------------------------------------------------------------------------------------
    // Per-layer resolution
    // -------------------------------------------------------------------------------------

    /// **The layer's own dictionary, and nothing else would be correct.** The two layers below
    /// number `beta` differently — 1 in the base, 0 in the extent — which is what independent
    /// per-layer numbering produces. Resolving once and scanning both layers with that one ordinal
    /// returns the wrong entities from one of them; this asserts the right ones from both.
    #[test]
    fn a_needle_resolves_against_each_layers_own_dictionary() {
        let columns = keyword_column(
            "sub",
            vec![
                // Base: alpha = 0, beta = 1. Entities 0..3.
                (None, universal(&[0, 1, 0]), dict(&["alpha", "beta"])),
                // Extent: beta = 0, gamma = 1. Entities 10, 11.
                (
                    Some("extents/f1.arrow"),
                    partial(&[10, 11], &[1, 0]),
                    dict(&["beta", "gamma"]),
                ),
            ],
        );
        let candidate = set(&[0, 1, 2, 10, 11]);
        let hits = columns
            .resolve("sub", &FilterOperand::TextEquals("beta".into()), &candidate)
            .unwrap();
        assert_eq!(members(&hits), vec![1, 11]);

        // `gamma` exists only in the extent's dictionary; the base's resolve misses and still
        // scans, contributing nothing.
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextEquals("gamma".into()),
                &candidate,
            )
            .unwrap();
        assert_eq!(members(&hits), vec![10]);

        // A needle no layer holds is an ordinary empty answer, not a refusal.
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextEquals("omega".into()),
                &candidate,
            )
            .unwrap();
        assert!(hits.is_empty());
    }

    /// The result is a subset of the candidate whatever the operand — **I12** as a property of the
    /// shape, checked here for the family whose leaves are new.
    #[test]
    fn a_keyword_leaf_never_widens_the_candidate() {
        let columns = keyword_column(
            "sub",
            vec![(None, universal(&[0, 1, 0, 1]), dict(&["alpha", "beta"]))],
        );
        let candidate = set(&[1, 3]);
        for operand in [
            FilterOperand::TextEquals("alpha".into()),
            FilterOperand::TextIn(vec!["alpha".into(), "beta".into()]),
            FilterOperand::TextPrefix("".into()),
            FilterOperand::TextContains("a".into()),
        ] {
            let hits = columns.resolve("sub", &operand, &candidate).unwrap();
            assert!(
                hits.and(&candidate) == hits,
                "{operand:?} escaped the candidate"
            );
        }
    }

    /// `prefix` is the contiguous ordinal range sortedness gives it, tested by the range scan.
    #[test]
    fn a_prefix_is_a_contiguous_ordinal_range() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 3]),
                dict(&["alpha", "alpine", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let hits = columns
            .resolve("sub", &FilterOperand::TextPrefix("alp".into()), &candidate)
            .unwrap();
        assert_eq!(members(&hits), vec![0, 1]);
        // The empty prefix is every key, which is every entity carrying a value — the honest
        // reading, and not every entity.
        let hits = columns
            .resolve("sub", &FilterOperand::TextPrefix("".into()), &candidate)
            .unwrap();
        assert_eq!(members(&hits), vec![0, 1, 2, 3]);
    }

    /// `in` over a keyword column is `eq` over a list, and a set naming values nobody holds costs
    /// the same shape of answer as one naming values everybody does.
    #[test]
    fn an_in_set_unions_its_needles() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 0]),
                dict(&["alpha", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextIn(vec!["gamma".into(), "alpha".into(), "nope".into()]),
                &candidate,
            )
            .unwrap();
        assert_eq!(members(&hits), vec![0, 2, 3]);
    }

    // -------------------------------------------------------------------------------------
    // `contains`, and its two routes
    // -------------------------------------------------------------------------------------

    /// **The two routes must agree, or the crossover is a correctness switch rather than a cost
    /// one.** Run over a partial column with a scattered presence and a scattered candidate, which
    /// is the shape the narrow route's slot arithmetic is most likely to get wrong.
    #[test]
    fn the_two_contains_routes_agree() {
        let keys = [
            "arxiv/0001",
            "arxiv/0002",
            "arxiv/1001",
            "bio/0001",
            "bio/2002",
            "cs/0003",
            "cs/1001",
            "math/0001",
        ];
        let d = dict(&keys);
        let entities = [1u32, 2, 5, 9, 40, 41, 42, 100_000, 100_001, 200_000];
        let ordinals = [0u32, 3, 6, 1, 7, 2, 4, 5, 0, 6];
        let values = partial(&entities, &ordinals);
        for candidate in [
            set(&entities),
            set(&[1, 42, 200_000]),
            set(&[5]),
            set(&[7, 8]),
            Bitmap::new(),
        ] {
            for needle in ["1001", "arxiv", "0001", "zzz", "/", "math/0001"] {
                let broad = contains_broad(&values, &d, needle, &candidate).unwrap();
                let narrow = contains_narrow(&values, &d, needle, &candidate).unwrap();
                assert_eq!(
                    members(&broad),
                    members(&narrow),
                    "routes disagree on {needle:?} over {:?}",
                    members(&candidate)
                );
            }
        }
    }

    /// **`contains`' ordinal test is sized by the dictionary, not by what matched.**
    ///
    /// The traversal counter cannot see this one: `runs` and `slots` were already equal across
    /// needles, because both routes always scanned the whole candidate. What differed was the cost
    /// *per slot* — a sorted list of matching ordinals is `O(log k)`, and for `contains` that *k*
    /// is the number of dictionary keys carrying the substring: a corpus-wide count, including keys
    /// no visible entity carries, that a caller can move by choosing a fragment. A table over the
    /// ordinal domain is the same size and the same test whatever matched, which is what the three
    /// needles below assert directly, since no counter can.
    #[test]
    fn contains_tests_ordinals_through_a_table_sized_by_the_dictionary() {
        let keys: Vec<String> = (0..64).map(|i| format!("host-{i:03}.example")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let d = dict(&refs);

        // Nothing, one key, and every key — the span a fragment-guessing caller would sweep.
        for (needle, expected) in [("zzz", 0usize), ("host-007", 1), ("example", 64)] {
            let mut matched = tessera_filter::CodeSet::over_domain(d.len() - 1);
            let matcher = tessera_filter::KeyMatcher::new(needle);
            let mut hits = 0usize;
            d.walk(|ordinal, key| {
                if matcher.matches(key) {
                    matched.insert(ordinal);
                    hits += 1;
                }
            })
            .unwrap();
            assert_eq!(hits, expected, "needle {needle:?}");
            assert_eq!(
                matched.domain(),
                d.len() - 1,
                "needle {needle:?}: the table is sized by the dictionary, whatever matched"
            );
        }
    }

    /// **The two routes traverse alike**, and not merely answer alike. They differ in how the
    /// matching ordinal set is found — every key in the dictionary, or only the blocks holding the
    /// candidate's own values — and not at all in the scan that turns that set into an answer. So
    /// whichever the crossover picks, the traversal `take_scan_work` counts is the same one, and
    /// the narrow route is no longer the one keyword shape sitting outside that harness.
    ///
    /// This is what makes the crossover a *cost* choice with nothing else riding on it: a route
    /// rule that changed the observable work would be choosing a disclosure profile as well as a
    /// price.
    #[test]
    fn the_two_contains_routes_traverse_alike() {
        let d = dict(&[
            "arxiv/0001",
            "arxiv/1001",
            "bio/0001",
            "cs/0003",
            "math/0001",
        ]);
        let entities = [1u32, 2, 5, 9, 40, 41, 100_000];
        let ordinals = [0u32, 3, 1, 4, 2, 0, 3];
        let values = partial(&entities, &ordinals);
        let mut compared = 0;
        for candidate in [
            set(&entities),
            set(&[1, 41, 100_000]),
            set(&[5]),
            set(&[7, 8]),
        ] {
            for needle in ["0001", "arxiv", "zzz", "/", "math/0001", ""] {
                let _ = take_scan_work();
                let broad = contains_broad(&values, &d, needle, &candidate).unwrap();
                let broad_work = take_scan_work();
                let narrow = contains_narrow(&values, &d, needle, &candidate).unwrap();
                let narrow_work = take_scan_work();
                assert_eq!(
                    members(&broad),
                    members(&narrow),
                    "{needle:?} answers differ"
                );
                assert_eq!(
                    broad_work,
                    narrow_work,
                    "{needle:?} over {:?}: the routes traversed differently",
                    members(&candidate)
                );
                if broad_work.runs > 0 && broad_work.slots > 0 {
                    compared += 1;
                }
            }
        }
        assert!(
            compared > 0,
            "every comparison traversed nothing, so they all held vacuously. A --release build is \
             the ordinary cause — the counter is compiled under debug_assertions"
        );
    }

    /// The routes agree on a universal column too, where the entity id is the slot and the run
    /// merge has no presence bitmap to thread.
    #[test]
    fn the_two_contains_routes_agree_over_a_universal_column() {
        let d = dict(&["ab", "abc", "bc", "cd"]);
        let values = universal(&[0, 1, 2, 3, 1, 0]);
        for candidate in [set(&[0, 1, 2, 3, 4, 5]), set(&[1, 4]), set(&[5])] {
            for needle in ["b", "ab", "cd", "q"] {
                assert_eq!(
                    members(&contains_broad(&values, &d, needle, &candidate).unwrap()),
                    members(&contains_narrow(&values, &d, needle, &candidate).unwrap()),
                    "routes disagree on {needle:?}"
                );
            }
        }
    }

    /// A substring that spans an elided shared prefix is still found — the reason the broad route
    /// decodes every key rather than searching the file's bytes. `alphabet` front-codes against
    /// `alpha`, so `phab` exists in no stored suffix.
    #[test]
    fn contains_finds_a_substring_spanning_an_elided_prefix() {
        let d = dict(&["alpha", "alphabet"]);
        let values = universal(&[0, 1]);
        let candidate = set(&[0, 1]);
        assert_eq!(
            members(&contains_broad(&values, &d, "phab", &candidate).unwrap()),
            vec![1]
        );
        assert_eq!(
            members(&contains_narrow(&values, &d, "phab", &candidate).unwrap()),
            vec![1]
        );
    }

    /// `contains` matching no key is the empty answer by way of the sentinel, and the whole
    /// candidate is still scanned for it.
    #[test]
    fn a_contains_matching_no_key_still_scans() {
        let d = dict(&["alpha", "beta"]);
        let values = universal(&[0, 1]);
        assert!(contains_broad(&values, &d, "zzz", &set(&[0, 1]))
            .unwrap()
            .is_empty());
    }

    /// The empty needle is *carries a value in this column*, which is not every entity: entity 2
    /// below has no value and matches nothing.
    #[test]
    fn an_empty_contains_needle_is_carrying_a_value() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                partial(&[0, 1, 3], &[0, 1, 0]),
                dict(&["alpha", "beta"]),
            )],
        );
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextContains("".into()),
                &set(&[0, 1, 2, 3]),
            )
            .unwrap();
        assert_eq!(members(&hits), vec![0, 1, 3]);
    }

    /// **The crossover reads two numbers and neither is about content.** A candidate small against
    /// the dictionary takes the narrow route; one large against it takes the broad. The boundary is
    /// the ratio of the two measured constants, and the same request over the same mask takes the
    /// same route whatever the needle.
    #[test]
    fn the_contains_crossover_is_candidate_size_against_dictionary_size() {
        // 10³ candidate entities against 10⁹ keys: probing a thousand keys beats decoding a
        // billion.
        assert_eq!(contains_route(1_000, 1_000_000_000), ContainsRoute::Narrow);
        // The whole corpus against a small vocabulary: one pass over the dictionary, then a scan.
        assert_eq!(contains_route(1_000_000_000, 1_000), ContainsRoute::Broad);
        // The boundary itself, from the constants rather than from a remembered number.
        let keys = 1_000_000u64;
        let boundary = keys * BROAD_KEY_NS / NARROW_PROBE_NS;
        assert_eq!(contains_route(boundary - 1, keys), ContainsRoute::Narrow);
        assert_eq!(contains_route(boundary, keys), ContainsRoute::Broad);
        // An empty dictionary can only be walked — there is nothing to probe for.
        assert_eq!(contains_route(0, 0), ContainsRoute::Broad);
    }
}
