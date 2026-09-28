//! A grouping by the values of a category field: which values a table lists, and how many items of
//! a set carry each value.
//!
//! A set's values are counted by whichever route it affords: through the field's per-value
//! records where the set's entities are at hand, by one pass over the set's rows where the field
//! is drawn in the view, and otherwise through the entities of the set's rows. The three give the
//! same counts. A value is counted only where an item of the set carries it, so a `derived` value
//! no visible item carries has no count to show, in its own row or in the rest.

use rayon::prelude::*;
use rustc_hash::FxHashMap;
use tessera_store::manifest::Visibility;
use tessera_store::read::ScalarSlice;
use tessera_store::vocabulary::ABSENT_CODE;

use super::set::{Cx, Set};
use super::table::{Groups, Key};
use super::{AggregateRefused, AggregateTimings, Pick};
use crate::cells::{CellSet, GroupTable, RowGroups};
use crate::error::{EngineError, Result};
use crate::filter::{CountCodes, EntityCodes};
use crate::Generation;

/// Rows one piece of a pass over a drawn column reads.
const PIECE_ROWS: u32 = 1 << 20;

/// A category field as one request counts it.
pub(super) struct Field {
    column: String,
    vocabulary: String,
    /// The field's per-entity values are held, with per-value records where the build wrote them.
    held: bool,
    /// The field is drawn in every view's rows.
    drawn: bool,
    derived: bool,
    pick: Pick<String>,
}

impl Field {
    /// The field `column` names, where it can be counted.
    pub(super) fn of(generation: &Generation, column: &str, pick: &Pick<String>) -> Result<Field> {
        let manifest = &generation.bundle.manifest;
        let not_countable =
            || EngineError::AggregateRefused(AggregateRefused::NotCountable(column.to_string()));
        let vocabulary =
            crate::categories::vocabulary_of(manifest, column).ok_or_else(not_countable)?;
        let minter = generation
            .vocabularies
            .get(&vocabulary)
            .ok_or_else(not_countable)?;
        let held = generation.filter_columns.value_layers(column).is_some();
        let drawn = manifest
            .render_scalars()
            .any(|scalar| scalar.name == column);
        if !held && !drawn {
            return Err(not_countable());
        }
        Ok(Field {
            column: column.to_string(),
            derived: minter.visibility() == Visibility::Derived,
            vocabulary,
            held,
            drawn,
            pick: pick.clone(),
        })
    }

    /// Whether counting this field needs a set's rows rather than its entities.
    pub(super) fn wants_rows(&self) -> bool {
        !self.held
    }

    /// The table's groups under `cx`: the listed values, `chosen` where the table has begun, and
    /// each group's counts in the set and the reference.
    pub(super) fn groups(
        &self,
        cx: &Cx<'_>,
        chosen: Option<&[u64]>,
        timings: &mut AggregateTimings,
    ) -> Result<Groups> {
        let counting = std::time::Instant::now();
        let set = self.counts(cx, &cx.sets.set)?;
        let reference = match &cx.sets.reference {
            Some(reference) => Some(self.counts(cx, reference)?),
            None => None,
        };
        timings.count_ns += counting.elapsed().as_nanos() as u64;
        timings.entities_crossed +=
            cx.sets.set.crossed() + cx.sets.reference.as_ref().map_or(0, Set::crossed);
        let vocabulary = cx
            .generation
            .vocabularies
            .get(&self.vocabulary)
            .ok_or_else(|| {
                EngineError::AggregateRefused(AggregateRefused::NotCountable(self.column.clone()))
            })?;
        let listable = match self.pick {
            Pick::Named(_) => Some(self.listable(cx)?),
            Pick::Top(_) => None,
        };
        let listable = |code: u32| {
            listable
                .as_ref()
                .map_or(Ok(false), |listable| listable(code))
        };
        let codes: Vec<u32> = match chosen {
            Some(chosen) => chosen.iter().map(|&code| code as u32).collect(),
            None => match &self.pick {
                Pick::Top(n) => top(&set, *n as usize, &|code| vocabulary.key_of(code)),
                Pick::Named(keys) => {
                    let mut codes: Vec<u32> = Vec::with_capacity(keys.len());
                    for key in keys {
                        let Some(code) = vocabulary.code_of(key) else {
                            continue;
                        };
                        if !codes.contains(&code) && listable(code)? {
                            codes.push(code);
                        }
                    }
                    codes
                }
            },
        };
        let mut always = Vec::with_capacity(codes.len());
        for &code in &codes {
            always.push(listable(code)?);
        }
        let sizes = sizes(&codes, &set, reference.as_ref());
        let keys = codes
            .iter()
            .map(|&code| Key::Text(vocabulary.key_of(code).unwrap_or_default().to_string()))
            .collect();
        let titles = codes
            .iter()
            .map(|&code| {
                vocabulary
                    .key_of(code)
                    .and_then(|key| vocabulary.title_of(key))
                    .map(str::to_string)
            })
            .collect();
        Ok(Groups {
            chosen: codes.iter().map(|&code| u64::from(code)).collect(),
            sizes,
            always,
            keys,
            titles: Some(titles),
            distinct: set.by_code.len() as u64,
        })
    }

    /// Whether `/v1/categories` would list a value to this viewer: every value of a `public`
    /// vocabulary, and a `derived` value only where an item this viewer may see carries it.
    fn listable<'c>(&self, cx: &'c Cx<'_>) -> Result<impl Fn(u32) -> Result<bool> + 'c> {
        let membership = match self.derived {
            false => None,
            true => Some(
                cx.generation
                    .filter_columns
                    .category_membership(&self.column, &cx.sets.candidate)
                    .map_err(|e| EngineError::VocabularyVisibilityUnavailable {
                        column: self.column.clone(),
                        detail: e.to_string(),
                    })?,
            ),
        };
        let column = self.column.clone();
        Ok(move |code: u32| match &membership {
            None => Ok(true),
            Some(membership) => {
                membership
                    .carries(code)
                    .map_err(|e| EngineError::VocabularyVisibilityUnavailable {
                        column: column.clone(),
                        detail: e.to_string(),
                    })
            }
        })
    }

    /// How many items of `set` carry each value, and how many carry none.
    pub(super) fn counts(&self, cx: &Cx<'_>, set: &Set) -> Result<Counts> {
        cx.check_cancelled()?;
        if self.held && (set.has_entities() || !self.drawn) {
            return self.counts_by_entity(cx, set.entities(cx)?);
        }
        Ok(cx
            .engine
            .pool
            .install(|| tally_rows(set.cells(cx), cx.segments(), &self.column)))
    }

    fn counts_by_entity(&self, cx: &Cx<'_>, entities: &croaring::Bitmap) -> Result<Counts> {
        let generation = cx.generation;
        let vocabulary = generation.vocabularies.get(&self.vocabulary);
        let each = |visit: &mut dyn FnMut(u32)| {
            if let Some(vocabulary) = vocabulary {
                for (_, code) in vocabulary.bindings() {
                    visit(code);
                }
            }
        };
        let buffered = |visit: &mut dyn FnMut(u32, u32)| {
            crate::categories::buffered_codes(
                &generation.bundle.manifest,
                &generation.buffer,
                &self.column,
                visit,
            )
        };
        let counted = cx
            .engine
            .pool
            .install(|| {
                generation.filter_columns.category_counts(
                    &self.column,
                    entities,
                    CountCodes::All(&each),
                    &buffered,
                )
            })
            .map_err(|e| EngineError::VocabularyVisibilityUnavailable {
                column: self.column.clone(),
                detail: e.to_string(),
            })?;
        Ok(Counts {
            by_code: counted.nonzero(),
            none: counted.none().expect("every code was counted"),
        })
    }

    /// Where the pass reads each row's value, grouped by `table`.
    pub(super) fn row_groups<'s>(
        &self,
        cx: &'s Cx<'_>,
        table: &'s GroupTable,
        codes: &'s Option<EntityCodes<'s>>,
    ) -> Result<RowGroups<'s>> {
        if self.drawn {
            return Ok(RowGroups::drawn(cx.segments(), &self.column, table));
        }
        let tables = cx
            .open
            .served
            .data
            .row_space
            .row_entities()
            .ok_or_else(|| {
                EngineError::Malformed(format!(
                    "view '{}' has no row-to-entity table, so '{}' cannot be counted by cell; \
                 rebuild the bundle",
                    cx.open.served.name, self.column
                ))
            })?;
        let codes = codes.as_ref().expect("a held field's codes are read");
        Ok(RowGroups::entity(tables, codes, table))
    }

    /// The per-entity codes [`Self::row_groups`] reads where the field is not drawn.
    pub(super) fn entity_codes<'g>(
        &self,
        generation: &'g Generation,
    ) -> Result<Option<EntityCodes<'g>>> {
        if self.drawn {
            return Ok(None);
        }
        generation
            .filter_columns
            .entity_codes(&self.column)
            .map(Some)
            .map_err(|e| EngineError::Malformed(e.to_string()))
    }
}

/// How many items of one set carry each value.
pub(super) struct Counts {
    /// Every value carried, ascending by code, with its count.
    by_code: Vec<(u32, u64)>,
    /// Items carrying no value.
    none: u64,
}

impl Counts {
    fn get(&self, code: u32) -> u64 {
        self.by_code
            .binary_search_by_key(&code, |&(c, _)| c)
            .map_or(0, |i| self.by_code[i].1)
    }

    fn carried(&self) -> u64 {
        self.by_code.iter().map(|&(_, n)| n).sum()
    }
}

/// The `n` codes carried by the most items, ties by key.
fn top<'v>(counts: &Counts, n: usize, key_of: &dyn Fn(u32) -> Option<&'v str>) -> Vec<u32> {
    let mut ranked: Vec<(u64, u32)> = counts.by_code.iter().map(|&(code, c)| (c, code)).collect();
    ranked.sort_unstable_by_key(|&(count, _)| std::cmp::Reverse(count));
    let Some(&(floor, _)) = ranked.get(n.saturating_sub(1)).or(ranked.last()) else {
        return Vec::new();
    };
    ranked.retain(|&(count, _)| count >= floor);
    let mut keyed: Vec<(u64, &str, u32)> = ranked
        .into_iter()
        .map(|(count, code)| (count, key_of(code).unwrap_or_default(), code))
        .collect();
    keyed.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    keyed.into_iter().take(n).map(|(_, _, code)| code).collect()
}

/// Each group's counts in the set and the reference: the listed codes in order, then the rest,
/// then none.
fn sizes(codes: &[u32], set: &Counts, reference: Option<&Counts>) -> Vec<(u64, u64)> {
    let of = |counts: Option<&Counts>| -> Vec<u64> {
        let Some(counts) = counts else {
            return vec![0; codes.len() + 2];
        };
        let listed: Vec<u64> = codes.iter().map(|&code| counts.get(code)).collect();
        let rest = counts.carried() - listed.iter().sum::<u64>();
        listed.into_iter().chain([rest, counts.none]).collect()
    };
    of(Some(set)).into_iter().zip(of(reference)).collect()
}

/// A drawn column's codes over a set's rows, counted in one parallel pass. Code 0 and a segment
/// without the column count as no value.
fn tally_rows(
    set: CellSet<'_>,
    segments: &[(&tessera_store::read::SegmentData, u32)],
    column: &str,
) -> Counts {
    let pieces: Vec<(usize, std::ops::Range<u32>)> = segments
        .iter()
        .enumerate()
        .flat_map(|(s, &(segment, row_base))| {
            (0..segment.row_count.div_ceil(PIECE_ROWS)).map(move |p| {
                let lo = row_base + p * PIECE_ROWS;
                (s, lo..(lo + PIECE_ROWS).min(row_base + segment.row_count))
            })
        })
        .collect();
    // Indexed by code up to the widest code any segment stores, a map where one stores 32 bits.
    let width = segments
        .iter()
        .map(|(segment, _)| match segment.columns.scalar(column) {
            Some(ScalarSlice::U8(_)) => Some(1 << 8),
            Some(ScalarSlice::U16(_)) => Some(1 << 16),
            Some(ScalarSlice::U32(_)) => None,
            _ => Some(1),
        })
        .try_fold(1usize, |most, width| width.map(|w| most.max(w)))
        .unwrap_or(0);
    let empty = || Tally::new(width);
    let tally = pieces
        .par_iter()
        .fold(empty, |mut tally, (s, rows)| {
            let (segment, row_base) = segments[*s];
            let base = row_base as usize;
            match segment.columns.scalar(column) {
                Some(ScalarSlice::U8(codes)) => set.for_each_run(rows.clone(), &mut |run| {
                    for row in run {
                        tally.add(u32::from(codes[row as usize - base]));
                    }
                }),
                Some(ScalarSlice::U16(codes)) => set.for_each_run(rows.clone(), &mut |run| {
                    for row in run {
                        tally.add(u32::from(codes[row as usize - base]));
                    }
                }),
                Some(ScalarSlice::U32(codes)) => set.for_each_run(rows.clone(), &mut |run| {
                    for row in run {
                        tally.add(codes[row as usize - base]);
                    }
                }),
                _ => tally.absent += set.count(rows.clone()),
            }
            tally
        })
        .reduce(empty, Tally::merge);
    tally.into_counts()
}

/// A running count per code: indexed by code for a `u8` or `u16` column, a map for a `u32` one.
struct Tally {
    dense: Vec<u64>,
    sparse: FxHashMap<u32, u64>,
    /// Rows of segments without the column.
    absent: u64,
}

impl Tally {
    /// A tally indexed by code below `width`, and a map above it.
    fn new(width: usize) -> Tally {
        Tally {
            dense: vec![0; width],
            sparse: FxHashMap::default(),
            absent: 0,
        }
    }

    #[inline]
    fn add(&mut self, code: u32) {
        match self.dense.get_mut(code as usize) {
            Some(slot) => *slot += 1,
            None => *self.sparse.entry(code).or_default() += 1,
        }
    }

    fn merge(mut self, other: Tally) -> Tally {
        if self.dense.len() < other.dense.len() {
            return other.merge(self);
        }
        for (slot, n) in self.dense.iter_mut().zip(&other.dense) {
            *slot += n;
        }
        for (code, n) in other.sparse {
            *self.sparse.entry(code).or_default() += n;
        }
        self.absent += other.absent;
        self
    }

    fn into_counts(self) -> Counts {
        let mut by_code: Vec<(u32, u64)> = self
            .dense
            .iter()
            .enumerate()
            .map(|(code, &n)| (code as u32, n))
            .chain(self.sparse)
            .filter(|&(_, n)| n > 0)
            .collect();
        by_code.sort_unstable();
        let mut none = self.absent;
        if let Some(&(ABSENT_CODE, n)) = by_code.first() {
            none += n;
            by_code.remove(0);
        }
        Counts { by_code, none }
    }
}
