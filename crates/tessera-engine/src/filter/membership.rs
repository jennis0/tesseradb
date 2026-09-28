use croaring::Bitmap;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use tessera_filter::{Codes, ColumnPostings, ValueColumn};
use tessera_store::vocabulary::ABSENT_CODE;
use tessera_types::AttrLocalId;

use super::columns::{Column, FilterColumns};
use super::error::FilterError;
use super::expr::UNRESOLVABLE_VALUE;

impl FilterColumns {
    /// The membership question `/v1/categories` asks of a `derived` column: which of this
    /// column's values does at least one entity in `candidate` carry? Derived, never maintained,
    /// and evaluated entirely inside the composed verdict.
    ///
    /// The postings are the base build's, so the extents are swept here, once, collecting the
    /// codes the candidate's extent entities carry; the per-value test is then a bitmap
    /// intersection against the postings plus a set lookup.
    pub fn category_membership<'a>(
        &'a self,
        column: &str,
        candidate: &'a Bitmap,
    ) -> Result<CategoryMembership<'a>, FilterError> {
        let held = self
            .columns
            .get(column)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        let layers = held.value_layers();
        // A column with no base holds only extents, so the sweep is the whole answer. A base
        // layer with no postings has no member set to read.
        let holds_base = layers.iter().any(|l| l.values_rel.is_none());
        let postings: Option<&ColumnPostings> = match held.postings() {
            Some(postings) => Some(postings),
            None if !holds_base => None,
            None => return Err(FilterError::MembershipUnavailable(column.to_string())),
        };

        let mut from_extents: FxHashSet<u32> = FxHashSet::default();
        for layer in layers.iter().filter(|l| l.values_rel.is_some()) {
            layer.values.for_each_code_in(candidate, |_, code| {
                from_extents.insert(code);
            });
        }
        Ok(CategoryMembership {
            column: column.to_string(),
            postings,
            candidate,
            from_extents,
        })
    }

    /// How many entities of `set` carry each of `codes` in `column`: the base, every extent and
    /// the buffered rows `buffered` visits, which are disjoint in entity space and so add.
    ///
    /// `set` must come from inside the viewer's visible set, a candidate or a part of one; the
    /// counts are over `set` and nothing else. `buffered` calls its argument with `(entity, code)`
    /// for each buffered row carrying a value in this column, and entities outside `set` are
    /// skipped here. Code 0 is the absent value and is never counted as a value.
    pub fn category_counts(
        &self,
        column: &str,
        set: &Bitmap,
        codes: CountCodes<'_>,
        buffered: &VisitBuffered<'_>,
    ) -> Result<CategoryCounts, FilterError> {
        let held = self
            .columns
            .get(column)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        let layers = held.value_layers();
        let empty = match codes {
            CountCodes::Only(listed) => Tally::only(listed),
            CountCodes::All(_) => Tally::for_width(layers.first().map(|l| l.values.codes())),
        };
        let mut tally = empty.clone();
        for layer in layers {
            match (layer.values_rel.is_none(), counted_through(held)) {
                (true, Some(postings)) => {
                    let mut failed = None;
                    let mut read = |code: u32| {
                        if code == ABSENT_CODE || failed.is_some() {
                            return;
                        }
                        match postings.intersection_cardinality(AttrLocalId::new(code), set) {
                            Ok(n) => tally.add(code, n),
                            Err(e) => failed = Some(e),
                        }
                    };
                    match codes {
                        CountCodes::All(each) => each(&mut read),
                        CountCodes::Only(_) => empty.listed().for_each(&mut read),
                    }
                    if let Some(e) = failed {
                        return Err(FilterError::postings_unreadable(column, e));
                    }
                }
                _ => tally.merge(scan(&layer.values, set, &empty)),
            }
        }
        buffered(&mut |entity, code| {
            if set.contains(entity) {
                tally.add(code, 1);
            }
        });
        let none = match codes {
            CountCodes::All(_) => Some(set.cardinality().saturating_sub(tally.total())),
            CountCodes::Only(_) => None,
        };
        Ok(CategoryCounts { tally, none })
    }
}

/// The base layer's postings where [`FilterColumns::category_counts`] reads them, or `None` where
/// it scans the base over the set: postings wherever the column has them, whatever its
/// vocabulary's visibility. The filter route's choice is the column's own `Route`.
fn counted_through(column: &Column) -> Option<&ColumnPostings> {
    column.postings()
}

/// Every code `values` holds for an entity of `set`, counted in parallel over slices of 2^20
/// entities on the current rayon pool, one tally per slice.
fn scan(values: &ValueColumn, set: &Bitmap, empty: &Tally) -> Tally {
    const SLICE_SHIFT: u32 = 20;
    let (Some(lo), Some(hi)) = (set.minimum(), set.maximum()) else {
        return empty.clone();
    };
    let count = |part: &Bitmap| {
        let mut tally = empty.clone();
        values.for_each_code_in(part, |_, code| tally.add(code, 1));
        tally
    };
    if lo >> SLICE_SHIFT == hi >> SLICE_SHIFT {
        return count(set);
    }
    ((lo >> SLICE_SHIFT)..=(hi >> SLICE_SHIFT))
        .into_par_iter()
        .map(|slice| {
            let first = slice << SLICE_SHIFT;
            let mut range = Bitmap::new();
            range.add_range(first..=first | ((1 << SLICE_SHIFT) - 1));
            count(&set.and(&range))
        })
        .reduce(
            || empty.clone(),
            |mut a, b| {
                a.merge(b);
                a
            },
        )
}

/// Calls its argument with each code a vocabulary binds.
pub type VisitCodes<'a> = dyn Fn(&mut dyn FnMut(u32)) + 'a;

/// Calls its argument with `(entity, code)` for each buffered row holding a value in one column.
pub type VisitBuffered<'a> = dyn Fn(&mut dyn FnMut(u32, u32)) + 'a;

/// Which codes [`FilterColumns::category_counts`] counts.
pub enum CountCodes<'a> {
    /// Every code. The caller visits each code the vocabulary binds, which is what a read of the
    /// base's postings counts, one posting per code.
    All(&'a VisitCodes<'a>),
    /// These codes and no others, each counted once however often it is listed.
    Only(&'a [u32]),
}

/// Per-code counts over one set, from [`FilterColumns::category_counts`].
pub struct CategoryCounts {
    tally: Tally,
    none: Option<u64>,
}

impl CategoryCounts {
    /// How many entities of the set carry `code`: 0 for a code no entity of the set carries, and
    /// for a code outside a [`CountCodes::Only`] list.
    pub fn get(&self, code: u32) -> u64 {
        self.tally.get(code)
    }

    /// Every counted code with at least one entity, and its count, ascending by code.
    pub fn nonzero(&self) -> Vec<(u32, u64)> {
        let mut out: Vec<(u32, u64)> = match &self.tally {
            Tally::Dense(counts) => counts
                .iter()
                .enumerate()
                .map(|(code, &n)| (code as u32, n))
                .collect(),
            Tally::Sparse(counts) | Tally::Only(counts) => {
                counts.iter().map(|(&code, &n)| (code, n)).collect()
            }
        };
        out.retain(|&(_, n)| n > 0);
        out.sort_unstable();
        out
    }

    /// How many distinct codes the set carries among those counted.
    pub fn distinct(&self) -> usize {
        self.nonzero().len()
    }

    /// How many entities of the set carry no value, where every code was counted; `None` for a
    /// [`CountCodes::Only`] count, which does not read the others.
    pub fn none(&self) -> Option<u64> {
        self.none
    }
}

/// A running count per code: a table indexed by code for a `u8` or `u16` column, a map for a
/// `u32` one, and a map fixed to its keys for a named list.
#[derive(Clone)]
enum Tally {
    Dense(Vec<u64>),
    Sparse(FxHashMap<u32, u64>),
    Only(FxHashMap<u32, u64>),
}

impl Tally {
    fn for_width(codes: Option<&Codes>) -> Tally {
        match codes {
            Some(Codes::U8(_)) => Tally::Dense(vec![0; 1 << 8]),
            Some(Codes::U16(_)) => Tally::Dense(vec![0; 1 << 16]),
            _ => Tally::Sparse(FxHashMap::default()),
        }
    }

    fn only(listed: &[u32]) -> Tally {
        Tally::Only(
            listed
                .iter()
                .filter(|&&code| code != ABSENT_CODE)
                .map(|&code| (code, 0))
                .collect(),
        )
    }

    /// The codes of a named list.
    fn listed(&self) -> impl Iterator<Item = u32> + '_ {
        let listed = match self {
            Tally::Only(counts) => Some(counts.keys().copied()),
            Tally::Dense(_) | Tally::Sparse(_) => None,
        };
        listed.into_iter().flatten()
    }

    #[inline]
    fn add(&mut self, code: u32, n: u64) {
        if code == ABSENT_CODE {
            return;
        }
        match self {
            Tally::Dense(counts) => {
                if let Some(count) = counts.get_mut(code as usize) {
                    *count += n;
                }
            }
            Tally::Sparse(counts) => *counts.entry(code).or_default() += n,
            Tally::Only(counts) => {
                if let Some(count) = counts.get_mut(&code) {
                    *count += n;
                }
            }
        }
    }

    fn merge(&mut self, other: Tally) {
        match other {
            Tally::Dense(counts) => {
                for (code, n) in counts.into_iter().enumerate().filter(|&(_, n)| n > 0) {
                    self.add(code as u32, n);
                }
            }
            Tally::Sparse(counts) | Tally::Only(counts) => {
                for (code, n) in counts {
                    self.add(code, n);
                }
            }
        }
    }

    fn get(&self, code: u32) -> u64 {
        match self {
            Tally::Dense(counts) => counts.get(code as usize).copied().unwrap_or(0),
            Tally::Sparse(counts) | Tally::Only(counts) => counts.get(&code).copied().unwrap_or(0),
        }
    }

    fn total(&self) -> u64 {
        match self {
            Tally::Dense(counts) => counts.iter().sum(),
            Tally::Sparse(counts) | Tally::Only(counts) => counts.values().sum(),
        }
    }
}

/// One column's value-visibility predicate for one principal, at one generation.
///
/// Built by [`FilterColumns::category_membership`]; see its doc for why the extents are swept up
/// front and the postings probed per value.
pub struct CategoryMembership<'a> {
    column: String,
    /// The base build's postings, `None` for a column with no base yet.
    postings: Option<&'a ColumnPostings>,
    candidate: &'a Bitmap,
    /// The codes the candidate's post-build entities carry, the half no posting covers.
    from_extents: FxHashSet<u32>,
}

impl CategoryMembership<'_> {
    /// The column this predicate was built for, for a caller shaping a refusal that names it.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// Is `code` carried by at least one entity this principal may see? The extent half is
    /// answered first, a hash lookup already in hand.
    pub fn carries(&self, code: u32) -> Result<bool, FilterError> {
        if code == UNRESOLVABLE_VALUE.raw() {
            // The reserved absent sentinel, carried by exactly the entities that carry no value:
            // it is not a value and is never visible.
            return Ok(false);
        }
        if self.from_extents.contains(&code) {
            return Ok(true);
        }
        // A boolean against the mapped view, never a materialised posting: `intersects`
        // short-circuits at the first shared container and allocates nothing.
        let Some(postings) = self.postings else {
            return Ok(false);
        };
        postings
            .intersects(AttrLocalId::new(code), self.candidate)
            .map_err(|e| FilterError::postings_unreadable(&self.column, e))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use croaring::Bitmap;
    use tessera_filter::{Codes, ColumnPostings, ValueColumn};

    use super::super::columns::{Column, FilterColumns, Layer, Route};
    use super::super::declared::Family;
    use super::{CategoryCounts, CountCodes};

    /// A `u16` category column: a base over entities `0..base.len()` with its postings where
    /// `postings` says so, and one extent over `extent`'s entities.
    fn column(
        dir: &std::path::Path,
        base: &[u16],
        postings: bool,
        extent: &[(u32, u16)],
    ) -> FilterColumns {
        let base_values = Arc::new(ValueColumn::universal(Codes::U16(base.to_vec().into())));
        let postings = postings.then(|| {
            let mut by_code: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            for (entity, &code) in base.iter().enumerate() {
                if code != 0 {
                    by_code
                        .entry(u32::from(code))
                        .or_default()
                        .push(entity as u32);
                }
            }
            let path = dir.join("postings.arrow");
            let records: Vec<(u32, Vec<u32>)> = by_code.into_iter().collect();
            tessera_authz::write_delta_tier_at(&path, &records, 32).expect("postings write");
            Arc::new(ColumnPostings::open_keyed(&path).expect("postings open"))
        });
        let mut held = Column::values(
            0,
            true,
            Family::Category,
            Some(Layer {
                values_rel: None,
                values: base_values,
                dict: None,
            }),
            postings,
            Route::Scan,
        );
        let codes: Vec<u16> = extent.iter().map(|&(_, code)| code).collect();
        let entities: Vec<u32> = extent.iter().map(|&(entity, _)| entity).collect();
        held.push_extent(
            "c",
            "extent-1",
            Arc::new(
                ValueColumn::partial(Codes::U16(codes.into()), Bitmap::of(&entities))
                    .expect("one code per entity"),
            ),
            None,
        )
        .expect("the extent is disjoint from the base");
        let mut columns = FilterColumns::default();
        columns.columns.insert("c".to_string(), held);
        columns
    }

    /// `(entity, code)` for every entity of the fixture, the base's then the extent's then the
    /// buffer's.
    fn every_value(base: &[u16], extent: &[(u32, u16)], buffer: &[(u32, u32)]) -> Vec<(u32, u32)> {
        base.iter()
            .enumerate()
            .map(|(e, &c)| (e as u32, u32::from(c)))
            .chain(extent.iter().map(|&(e, c)| (e, u32::from(c))))
            .chain(buffer.iter().copied())
            .collect()
    }

    fn counts(
        columns: &FilterColumns,
        set: &Bitmap,
        codes: CountCodes<'_>,
        buffer: &[(u32, u32)],
    ) -> CategoryCounts {
        columns
            .category_counts("c", set, codes, &|visit| {
                for &(entity, code) in buffer {
                    visit(entity, code);
                }
            })
            .expect("the column counts")
    }

    /// A count over a base, an extent and a buffer is the number of the set's entities carrying
    /// each code, through the postings and through a scan alike, for every code and for a named
    /// list; the absent code is never a value, and `none` is the rest of the set.
    #[test]
    fn a_count_adds_the_base_the_extents_and_the_buffer() {
        let base: Vec<u16> = (0..3_000u32)
            .map(|e| [0, 7, 40_000, 7, 9][e as usize % 5])
            .collect();
        let extent: Vec<(u32, u16)> = (5_000..5_400)
            .map(|e| (e, [9, 12, 0][e as usize % 3]))
            .collect();
        let buffer: Vec<(u32, u32)> = (6_000..6_050)
            .map(|e| (e, [7, 0, 13][e as usize % 3]))
            .collect();
        let all = every_value(&base, &extent, &buffer);
        let sets = [
            Bitmap::from_range(0..7_000),
            Bitmap::of(&[1, 2, 3, 5_001, 5_002, 6_000, 6_002, 6_003]),
            (0..7_000u32).filter(|e| e % 7 == 0).collect(),
            Bitmap::new(),
        ];
        let bindings = [7u32, 9, 12, 13, 40_000, 55];
        for postings in [true, false] {
            let dir = tempfile::tempdir().expect("tempdir");
            let columns = column(dir.path(), &base, postings, &extent);
            for set in &sets {
                let want = |code: u32| {
                    all.iter()
                        .filter(|&&(e, c)| c == code && set.contains(e))
                        .count() as u64
                };
                let every = counts(
                    &columns,
                    set,
                    CountCodes::All(&|visit| bindings.iter().for_each(|&c| visit(c))),
                    &buffer,
                );
                for code in bindings {
                    assert_eq!(
                        every.get(code),
                        want(code),
                        "postings {postings}, code {code}"
                    );
                }
                let carried: u64 = bindings.iter().map(|&code| want(code)).sum();
                assert_eq!(every.none(), Some(set.cardinality() - carried));
                assert_eq!(
                    every.nonzero(),
                    bindings
                        .iter()
                        .map(|&c| (c, want(c)))
                        .filter(|&(_, n)| n > 0)
                        .collect::<Vec<_>>()
                );

                let named = counts(&columns, set, CountCodes::Only(&[9, 13, 9, 0, 55]), &buffer);
                for code in [9, 13, 55] {
                    assert_eq!(named.get(code), want(code), "named, postings {postings}");
                }
                assert_eq!(named.get(7), 0, "a code outside the list is not counted");
                assert_eq!(named.get(0), 0, "the absent code is not a value");
                assert_eq!(named.none(), None);
            }
        }
    }

    /// A set spanning several 2^20-entity slices counts as one spanning a single slice would.
    #[test]
    fn a_scan_over_many_slices_counts_every_slice_once() {
        let base: Vec<u16> = (0..3_500_000u32).map(|e| (e % 4) as u16).collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let columns = column(dir.path(), &base, false, &[]);
        let set: Bitmap = (0..3_500_000u32).filter(|e| e % 3 == 0).collect();
        let got = counts(
            &columns,
            &set,
            CountCodes::All(&|visit| (1..4).for_each(visit)),
            &[],
        );
        for code in 1..4u32 {
            let want = (0..3_500_000u32)
                .filter(|e| e % 3 == 0 && e % 4 == code)
                .count() as u64;
            assert_eq!(got.get(code), want, "code {code}");
        }
    }
}
