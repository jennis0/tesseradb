use std::ops::Range;

use croaring::Bitmap;
use tessera_filter::{CodeSet, Codes, DictError, KeyMatcher, SortedDict, ValueColumn};
use tessera_types::AttrLocalId;

use crate::filter::expr::FilterOperand;
use crate::filter::{Endpoint, Scalar};

/// The ordinal a needle no dictionary resolves is scanned for.
///
/// No dictionary can mint it: `SortedDictWriter` refuses the `u32::MAX`-th key, so a dictionary's
/// ordinals run `0..key_count` with `key_count <= u32::MAX`. A slot cannot hold it, so a scan for
/// it matches nothing while still walking every entity in the candidate.
pub(in crate::filter) const NO_SUCH_ORDINAL: u32 = u32::MAX;

/// What one layer's dictionary turns a keyword operand into: a question about ordinals the
/// fixed-width scan can answer.
///
/// Total on purpose: there is no variant meaning "matches nothing, so do not scan". A needle the
/// layer does not hold becomes [`NO_SUCH_ORDINAL`] and a prefix no key carries becomes the
/// sentinel range; every arm of [`scan_ordinals`] scans the whole candidate regardless, so a
/// dictionary miss costs what a hit costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::filter) enum OrdinalPredicate {
    /// One ordinal, `eq`, resolved.
    Eq(u32),
    /// A list of ordinals, `in`, one entry per needle the caller named and nothing else.
    In(Vec<u32>),
    /// A contiguous ordinal range, inclusive at both ends: what a prefix's dictionary range
    /// becomes. Inclusive because a half-open empty range would skip the scan:
    /// `SortedDict::prefix_range` answers "no key carries this" with `0..0` where the prefix sorts
    /// below every key, and carried through as an exclusive upper bound the range scan would find
    /// it unrepresentable and return without scanning. The sentinel range
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

/// Resolve one operand against one layer's dictionary.
///
/// A miss is ordinary and is not an error: `SortedDict::resolve` says so, and the whole point of
/// [`NO_SUCH_ORDINAL`] is that the scan proceeds. Only a malformed dictionary refuses. `in` yields
/// exactly one ordinal per needle named, misses included, so the list handed to the scan has the
/// caller's own length.
pub(in crate::filter) fn keyword_ordinals(
    dict: &SortedDict,
    operand: &FilterOperand,
) -> Result<OrdinalPredicate, DictError> {
    Ok(match operand {
        // `match` reaches no keyword layer through the parse; the sentinel is the fail-closed
        // reading if one arrives anyway.
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
        // The remaining operands belong to other families, refused by the parse on a keyword
        // column; the sentinel is the fail-closed answer if one arrives anyway.
        FilterOperand::TextContains(_)
        | FilterOperand::Equals(_)
        | FilterOperand::In(_)
        | FilterOperand::NumEquals(_)
        | FilterOperand::NumIn(_)
        | FilterOperand::Range { .. } => OrdinalPredicate::Eq(NO_SUCH_ORDINAL),
    })
}

/// One ordinal predicate against one layer's `u32` ordinal column. Every arm scans; none may learn
/// to return early.
///
/// The ordinals are handed to the value column as `AttrLocalId`, minted from this layer's
/// dictionary and consumed by this layer's column alone: a comparand for the width of one call,
/// not an identity.
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

/// Which of `contains`' two routes a layer takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::filter) enum ContainsRoute {
    /// Decode and substring-search every key in the layer's dictionary, collect the matching
    /// ordinals, then scan for them: a per-key loop, since front coding elides shared prefixes and
    /// a flat search of the file's bytes would miss a substring spanning an elided one.
    Broad,
    /// Take each candidate entity's ordinal and probe the dictionary for that one key: one random
    /// access per candidate entity, none for keys no visible entity carries.
    Narrow,
}

/// Per-key cost of the broad route's walk, in nanoseconds. Measured at 11.0-18.8 ns to decode one
/// key over three real arXiv columns; 15 is the middle of that band, decode alone, understating
/// the broad route's true cost.
const BROAD_KEY_NS: u64 = 15;

/// Per-candidate upper bound on the narrow route's cost, in nanoseconds. 0.10 us is one
/// `SortedDict::key_of` at the shipped restart interval, the bottom of the modelled band; measured
/// cost ranges 19.4-59.8 ns across six real shapes at a 25% candidate, so this prices the route at
/// its worst case rather than its typical one, which would pick it exactly where it is worst.
const NARROW_PROBE_NS: u64 = 100;

/// Choose a `contains` route from the candidate's cardinality and the layer's dictionary size, and
/// nothing else.
///
/// `|candidate| * NARROW_PROBE_NS` against `|dictionary| * BROAD_KEY_NS`: the narrow route costs
/// one probe per candidate entity, the broad route one decode per key plus an ordinal scan the
/// narrow route does not run. The candidate's cardinality is the principal's own quantity; a
/// dictionary's key count is a property of the bundle, identical for every principal. Neither
/// depends on the needle: the same request over the same mask takes the same route whether the
/// value exists or not.
pub(in crate::filter) fn contains_route(candidate_entities: u64, dictionary_keys: u64) -> ContainsRoute {
    if candidate_entities.saturating_mul(NARROW_PROBE_NS)
        < dictionary_keys.saturating_mul(BROAD_KEY_NS)
    {
        ContainsRoute::Narrow
    } else {
        ContainsRoute::Broad
    }
}

/// `contains` against one keyword layer, by whichever route [`contains_route`] names. The
/// crossover's constants above are calibration; either route answers correctly whichever is
/// chosen.
fn keyword_contains(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // Every key contains the empty needle, so the answer is the whole ordinal range: carries a
    // value in this column. No key is read, since no key's content bears on the answer.
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
    // size and never the needle's selectivity. The matcher is built once outside it, since a
    // searcher constructed per key was measured at up to 47% of this route's cost.
    //
    // Matches go into a table over the dictionary's ordinals rather than a list: it bounds the
    // allocation (a one-byte needle over a near-unique vocabulary matches most of it) and makes
    // the scan that follows cost the same per slot however many keys matched.
    let mut matched = CodeSet::over_domain(dict.len().saturating_sub(1));
    let matcher = KeyMatcher::new(needle);
    dict.walk(|ordinal, key| {
        if matcher.matches(key) {
            matched.insert(ordinal);
        }
    })?;
    // No key matched: an empty table, and the scan still runs over every candidate slot exactly as
    // it does for a full one.
    Ok(values.scan_ordinal_set(candidate, &matched))
}

/// The narrow route: read only the dictionary the candidate's own values occupy.
///
/// The route reads the candidate's ordinals, not its entities: entities sharing a value name the
/// same ordinal, and `SortedDict::key_of` decodes a whole block prefix to return one key, so
/// probing per candidate entity would pay discarded decodes for every duplicate. Deduplicating
/// first and handing the sorted result to `SortedDict::walk_ordinals` pays each block once
/// instead, measured at 1.91-6.05x the probe-per-entity loop across six real shapes.
///
/// The route ends where the broad route ends, one `OrdinalPredicate::In` scan over the candidate,
/// differing only in how the matching ordinal set is computed: from the whole dictionary, or from
/// the blocks the candidate's own values sit in.
fn contains_narrow(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // A keyword layer's ordinals are `u32` by construction. Any other width means the column and
    // its declaration disagree; the fail-closed reading is that nothing matches.
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
    // The same table the broad route ends in: an empty one scans every candidate slot exactly as a
    // full one does.
    Ok(values.scan_ordinal_set(candidate, &matched))
}

#[cfg(test)]
mod tests {
    //! The keyword read route, over dictionaries and ordinal columns built in this process: cases
    //! like two layers numbering one key differently are cheaper to contrive here than as fixtures.
    //! The end-to-end pass over a built bundle is `tests/filtering.rs`'s.

    use super::*;
    use crate::filter::test_support::*;
    use crate::filter::FilterColumns;
    use tessera_filter::{take_scan_work, ScanWork};

    // The sentinel rule

    /// What each operand resolves to: a miss must be carried to the scan, not the answer.
    #[test]
    fn an_operand_resolves_to_the_predicate_the_scan_answers() {
        let d = dict(&["alpha", "alpine", "beta", "gamma"]);
        let sentinel_range = OrdinalPredicate::Range {
            lo: NO_SUCH_ORDINAL,
            hi: NO_SUCH_ORDINAL,
        };
        let cases = [
            (
                "a needle the dictionary holds",
                FilterOperand::TextEquals("beta".into()),
                OrdinalPredicate::Eq(2),
            ),
            (
                "a needle no dictionary holds",
                FilterOperand::TextEquals("delta".into()),
                OrdinalPredicate::Eq(NO_SUCH_ORDINAL),
            ),
            (
                "a prefix two keys carry",
                FilterOperand::TextPrefix("alp".into()),
                OrdinalPredicate::Range { lo: 0, hi: 1 },
            ),
            (
                "a prefix sorting above every key",
                FilterOperand::TextPrefix("zeta".into()),
                sentinel_range.clone(),
            ),
            (
                "a prefix sorting below every key",
                FilterOperand::TextPrefix("aa".into()),
                sentinel_range.clone(),
            ),
            (
                "a set of three needles, one of them a miss",
                FilterOperand::TextIn(vec!["gamma".into(), "delta".into(), "alpha".into()]),
                OrdinalPredicate::In(vec![3, NO_SUCH_ORDINAL, 0]),
            ),
            (
                "an operand from another family",
                FilterOperand::NumEquals(Scalar::Int(3)),
                OrdinalPredicate::Eq(NO_SUCH_ORDINAL),
            ),
        ];
        for (label, operand, expected) in cases {
            assert_eq!(keyword_ordinals(&d, &operand).unwrap(), expected, "{label}");
        }
    }

    /// A sentinel predicate is answered by a real scan: an early return on it returns nothing here.
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

    // The sentinel rule, asserted in work
    //
    // The tests above pin what a miss is and what a scan for it answers. These pin what it costs:
    // a needle no dictionary holds must be indistinguishable from one every dictionary holds in
    // work, not merely in outcome.
    //
    // The unit is traversed slots and runs, never elapsed time. A stopwatch assertion would be
    // flaky in exactly the direction that lets the channel reopen, going green on a loaded machine,
    // so what is compared is the count `tessera_filter::take_scan_work` reports.
    //
    // Each of these routes through `FilterColumns::resolve` rather than `scan_ordinals`, since the
    // early return this guards against can hide in the resolve, the predicate, the scan or the
    // per-layer loop, which no answer-level test can see.

    /// The work one operand costs over `column`, and the entities it returns.
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

    /// Every operand in `ops` traverses exactly what the first one does, and the first traverses
    /// something.
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

    /// `eq` costs the same whether or not the needle exists.
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
        // The held needle really does match, so this compares two scans that found different
        // things.
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

    /// Two layers that disagree about a key must both scan in full, or a layer skipped on a miss
    /// answers right for less work.
    #[test]
    fn eq_traverses_alike_where_the_layers_disagree() {
        let columns = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1, 0]), dict(&["alpha", "beta"])),
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

    /// A prefix matching nothing costs what a matching prefix costs, including the `0..0` case
    /// `OrdinalPredicate::Range`'s inclusive bound exists for.
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

    /// An `in` set costs the same however many of its needles resolve.
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

    /// `contains` too, here on the broad route: its walk reads every key whatever the needle.
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

    // Per-layer resolution

    /// A needle resolves against each layer's own dictionary: the two layers below number `beta`
    /// differently.
    #[test]
    fn a_needle_resolves_against_each_layers_own_dictionary() {
        let columns = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1, 0]), dict(&["alpha", "beta"])),
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

        // `gamma` exists only in the extent's dictionary; the base's resolve misses and scans.
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

    /// The result is a subset of the candidate whatever the operand.
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
        // The empty prefix is every key, every entity carrying a value, not every entity.
        let hits = columns
            .resolve("sub", &FilterOperand::TextPrefix("".into()), &candidate)
            .unwrap();
        assert_eq!(members(&hits), vec![0, 1, 2, 3]);
    }

    /// `in` over a keyword column is `eq` over a list.
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

    // `contains`, and its two routes

    /// The two routes must agree, or the crossover is a correctness switch, not a cost one: run
    /// over a scattered presence and candidate, the shape most likely to break the narrow route.
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

    /// The two routes traverse alike, not merely answer alike: a route rule that changed the
    /// observable work would choose a disclosure profile as well as a price.
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

    /// The routes agree on a universal column too, where the entity id is the slot.
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

    /// A substring spanning an elided front-coded prefix is still found: `phab` in `alphabet`.
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

    /// The empty needle is "carries a value in this column", not every entity.
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

    /// The crossover reads two numbers and neither is about content, at the ratio of the two
    /// measured constants.
    #[test]
    fn the_contains_crossover_is_candidate_size_against_dictionary_size() {
        assert_eq!(contains_route(1_000, 1_000_000_000), ContainsRoute::Narrow);
        assert_eq!(contains_route(1_000_000_000, 1_000), ContainsRoute::Broad);
        // The boundary itself, from the constants rather than a remembered number.
        let keys = 1_000_000u64;
        let boundary = keys * BROAD_KEY_NS / NARROW_PROBE_NS;
        assert_eq!(contains_route(boundary - 1, keys), ContainsRoute::Narrow);
        assert_eq!(contains_route(boundary, keys), ContainsRoute::Broad);
        // An empty dictionary can only be walked, there is nothing to probe for.
        assert_eq!(contains_route(0, 0), ContainsRoute::Broad);
    }
}
