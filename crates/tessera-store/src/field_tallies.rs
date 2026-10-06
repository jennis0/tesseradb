//! `field-tallies.bin` — per view, the figures of every number and timestamp field over the base
//! rows, one tally for each distinct list of index keys the rows' items carry.
//!
//! A viewer's visible base rows are the rows whose item carries a key the viewer's grant satisfies,
//! and items carrying the same key list are seen or not seen together. So a grant's figures over
//! the base are the merge of the tallies of the key lists it satisfies, and no request walks a base
//! row to find them. A build writes the file for each view it writes a base for, and so does a
//! fold, both through [`derive_view`], which also decides which fields are tallied.
//!
//! A tally holds the rows it covers, the rows with no value, the count of finite values, their
//! exact sum ([`Sum`]), and the [`RESERVE`] smallest and largest finite values with their rows, so
//! a request that subtracts denied rows can still name its extremes.
//!
//! # The format
//!
//! Little-endian, after the magic `TSFT0002`:
//!
//! ```text
//! fields   u32 F, then per field: name length u16, name, float u8
//! lists    u32 L, then per list: key count u32, keys u32 ascending
//! tallies  L x F, list-major: rows u64, none u64, count u64, sum, low count u8, (value 16 B,
//!          row u32) each, high likewise
//! sum      on an integer field an i128; on a float field two magnitudes, each its first word u8,
//!          its word count u8 and the words u64
//! ```
//!
//! A value is an `i128`, or an `f64`'s bits in the low eight bytes, as its field's float byte
//! says. The file is listed in the manifest's digests, and a file that does not decode to its end
//! is refused.

use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::path::Path;

use rayon::prelude::*;
use tessera_types::scalar::{Number, ScalarType};

pub use crate::exact_sum::{CompactSum, ExactSum, Trimmed};
use crate::manifest::DeclaredScalar;
use crate::read::ColumnsRef;
use crate::render_presence::RenderPresence;

/// The file's name in a view's directory.
pub const FIELD_TALLIES_FILE: &str = "field-tallies.bin";

const MAGIC: &[u8; 8] = b"TSFT0002";

/// How many of the most extreme values a tally keeps on each side.
pub const RESERVE: usize = 8;

/// Put `value` among `side`'s `keep` most extreme values, where it is one of them: the smallest
/// first, or with `high` the largest first, ties by where each was read. The one order every
/// tally keeps its extremes in.
#[inline]
pub fn keep_extreme<K: PartialOrd + Copy>(
    side: &mut Vec<(K, u32)>,
    keep: usize,
    value: (K, u32),
    high: bool,
) {
    let before = |a: &(K, u32), b: &(K, u32)| match high {
        false => a.0 < b.0 || (a.0 == b.0 && a.1 < b.1),
        true => a.0 > b.0 || (a.0 == b.0 && a.1 < b.1),
    };
    if side.len() == keep && side.last().is_none_or(|last| !before(&value, last)) {
        return;
    }
    let at = side.partition_point(|held| before(held, &value));
    side.insert(at, value);
    side.truncate(keep);
}

/// A tally's exact sum as it is held: an integer field's in an `i128`, which holds the sum of 2^32
/// values of any integer width, and a float field's as a [`CompactSum`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sum {
    Int(i128),
    Float(CompactSum),
}

impl Default for Sum {
    fn default() -> Self {
        Sum::Int(0)
    }
}

impl Sum {
    fn exact(&self) -> ExactSum {
        match self {
            Sum::Int(i) => {
                let mut out = ExactSum::default();
                out.add_int(*i);
                out
            }
            Sum::Float(compact) => compact.expand(),
        }
    }

    /// `self + other`.
    pub fn plus(&self, other: &Sum) -> Sum {
        match (self, other) {
            (Sum::Int(a), Sum::Int(b)) if a.checked_add(*b).is_some() => Sum::Int(a + b),
            _ => Sum::Float(self.exact().plus(&other.exact()).compact()),
        }
    }

    /// `self - other`.
    pub fn minus(&self, other: &Sum) -> Sum {
        match (self, other) {
            (Sum::Int(a), Sum::Int(b)) if a.checked_sub(*b).is_some() => Sum::Int(a - b),
            _ => Sum::Float(self.exact().minus(&other.exact()).compact()),
        }
    }

    /// The sum over `count`, rounded once to the nearest `f64`, a tie to even.
    pub fn mean(&self, count: u64) -> Option<f64> {
        self.exact().mean(count)
    }

    /// The sum over `count`, rounded once to the nearest integer, a tie to even.
    pub fn mean_whole(&self, count: u64) -> Option<i128> {
        self.exact().mean_whole(count)
    }
}

/// One set of rows' values of a field: how many rows it covers, how many hold no value, and the
/// count and exact sum of the finite values, with the most extreme of them, each with its row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FieldTally {
    pub rows: u64,
    pub none: u64,
    pub count: u64,
    pub sum: Sum,
    /// The smallest values, ascending.
    pub low: Vec<(Number, u32)>,
    /// The largest values, descending.
    pub high: Vec<(Number, u32)>,
}

impl FieldTally {
    /// Both tallies, keeping `keep` values on each side.
    pub fn merged(self, other: &FieldTally, keep: usize) -> FieldTally {
        let mut merge = TallyMerge::of(self, keep);
        merge.add(other);
        merge.finish()
    }
}

/// Tallies merged into one, their sums added at full width and compacted once at the end.
pub struct TallyMerge {
    keep: usize,
    tally: FieldTally,
    sum: Accumulated,
}

/// A sum as it is added to.
enum Accumulated {
    Int(i128),
    Exact(Box<ExactSum>),
}

impl Accumulated {
    fn add(&mut self, sum: &Sum) {
        match (&mut *self, sum) {
            (Accumulated::Int(a), Sum::Int(b)) if a.checked_add(*b).is_some() => *a += b,
            (Accumulated::Exact(a), Sum::Float(b)) => {
                **a = std::mem::take(&mut **a).plus(&b.expand());
            }
            (held, sum) => {
                let mut exact = match held {
                    Accumulated::Int(i) => Sum::Int(*i).exact(),
                    Accumulated::Exact(e) => std::mem::take(&mut **e),
                };
                exact = exact.plus(&sum.exact());
                *held = Accumulated::Exact(Box::new(exact));
            }
        }
    }

    fn finish(self) -> Sum {
        match self {
            Accumulated::Int(i) => Sum::Int(i),
            Accumulated::Exact(e) => Sum::Float(e.compact()),
        }
    }
}

impl TallyMerge {
    /// A merge starting from `tally`, keeping `keep` values on each side.
    pub fn of(mut tally: FieldTally, keep: usize) -> TallyMerge {
        let sum = match std::mem::take(&mut tally.sum) {
            Sum::Int(i) => Accumulated::Int(i),
            Sum::Float(c) => Accumulated::Exact(Box::new(c.expand())),
        };
        TallyMerge { keep, tally, sum }
    }

    /// Add `other`.
    pub fn add(&mut self, other: &FieldTally) {
        self.tally.rows += other.rows;
        self.tally.none += other.none;
        self.tally.count += other.count;
        self.sum.add(&other.sum);
        for &value in &other.low {
            keep_extreme(&mut self.tally.low, self.keep, value, false);
        }
        for &value in &other.high {
            keep_extreme(&mut self.tally.high, self.keep, value, true);
        }
    }

    pub fn finish(mut self) -> FieldTally {
        self.tally.sum = self.sum.finish();
        self.tally
    }
}

/// One field's values over the base rows, for [`derive`].
pub enum TallySource<'a> {
    /// A drawn column of the base segment's `columns.arrow`, read by row.
    Drawn,
    /// Values held per entity, visited over a set of entities: each one holding a value, with it.
    Held(&'a (dyn Fn(&croaring::Bitmap, &mut dyn FnMut(u32, Number)) + Sync)),
}

/// One field [`derive`] tallies.
pub struct TallyField<'a> {
    pub name: String,
    pub float: bool,
    pub source: TallySource<'a>,
}

/// A view's tallies, as [`read`] gives them back.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FieldTallies {
    /// Each field's name and whether its values are floats.
    pub fields: Vec<(String, bool)>,
    /// The distinct key lists, ascending.
    pub lists: Vec<Vec<u32>>,
    /// Per list, per field, in `fields`' order.
    pub tallies: Vec<Vec<FieldTally>>,
}

impl FieldTallies {
    /// The position of `field` among the fields tallied.
    pub fn field(&self, field: &str) -> Option<usize> {
        self.fields.iter().position(|(name, _)| name == field)
    }
}

/// Entities one piece of [`derive`] visits.
const PIECE_ENTITIES: u64 = 1 << 18;

/// One list's tally of one field while a piece adds rows to it.
struct Tallying {
    rows: u64,
    none: u64,
    count: u64,
    sum: Accumulated,
    low: Vec<(Number, u32)>,
    high: Vec<(Number, u32)>,
}

impl Tallying {
    fn new(float: bool) -> Self {
        Tallying {
            rows: 0,
            none: 0,
            count: 0,
            sum: match float {
                true => Accumulated::Exact(Box::default()),
                false => Accumulated::Int(0),
            },
            low: Vec::new(),
            high: Vec::new(),
        }
    }

    #[inline]
    fn add(&mut self, value: Option<Number>, row: u32) {
        self.rows += 1;
        let value = match value {
            None => {
                self.none += 1;
                return;
            }
            Some(Number::Float(f)) if !f.is_finite() => return,
            Some(value) => value,
        };
        self.count += 1;
        match (&mut self.sum, value) {
            (Accumulated::Int(sum), Number::Int(i)) => *sum += i,
            (Accumulated::Exact(sum), Number::Float(f)) => sum.add_float(f),
            (Accumulated::Exact(sum), Number::Int(i)) => sum.add_int(i),
            (sum @ Accumulated::Int(_), Number::Float(_)) => {
                sum.add(&Sum::Float(CompactSum::default()));
                if let (Accumulated::Exact(e), Number::Float(f)) = (sum, value) {
                    e.add_float(f);
                }
            }
        }
        keep_extreme(&mut self.low, RESERVE, (value, row), false);
        keep_extreme(&mut self.high, RESERVE, (value, row), true);
    }

    fn finish(self) -> FieldTally {
        FieldTally {
            rows: self.rows,
            none: self.none,
            count: self.count,
            sum: self.sum.finish(),
            low: self.low,
            high: self.high,
        }
    }
}

/// Tally `fields` over a view's base rows, grouping each row by its item's key list, and write the
/// result to `path`.
///
/// The rows are visited in entity order, a span of entities at a time, so nothing is asked of an
/// entity by rank: `row_of` names each entity's base row in the view, `lists` hands over every key
/// list held for the entities of a span in ascending order, and a held field hands over its values
/// over a set of entities the same way. An entity with a row and no list is tallied under the empty
/// list. `bound` is one past the highest entity the view's base holds. A drawn field is read from
/// `columns` by row.
pub fn derive(
    bound: u64,
    columns: Option<&ColumnsRef>,
    row_of: &(dyn Fn(u32) -> Option<u32> + Sync),
    lists: &(dyn Fn(Range<u32>, &mut dyn FnMut(u32, &[u32])) -> io::Result<()> + Sync),
    fields: &[TallyField<'_>],
    path: &Path,
) -> io::Result<FieldTallies> {
    // A drawn field's slice and presence, looked up once.
    let drawn: Vec<Option<(crate::read::ScalarSlice<'_>, &RenderPresence)>> = fields
        .iter()
        .map(|field| match (&field.source, columns) {
            (TallySource::Drawn, Some(columns)) => columns
                .scalar(&field.name)
                .map(|slice| (slice, columns.presence(&field.name))),
            _ => None,
        })
        .collect();
    type Groups = HashMap<Vec<u32>, Vec<FieldTally>>;
    let bound = bound.min(u64::from(u32::MAX) + 1);
    let pieces: Vec<u64> = (0..bound.div_ceil(PIECE_ENTITIES)).collect();
    let tallied: io::Result<Groups> = pieces
        .into_par_iter()
        .map(|piece| -> io::Result<Groups> {
            let start = (piece * PIECE_ENTITIES) as u32;
            let end = ((piece + 1) * PIECE_ENTITIES).min(bound);
            let span = start..(end - 1) as u32 + 1;
            let width = (end - u64::from(start)) as usize;
            // Each entity's row, where it has one in the view's base.
            let mut rows = vec![u32::MAX; width];
            let mut present = Vec::new();
            for entity in span.clone() {
                if let Some(row) = row_of(entity) {
                    rows[(entity - start) as usize] = row;
                    present.push(entity);
                }
            }
            if present.is_empty() {
                return Ok(Groups::new());
            }
            let present = croaring::Bitmap::of(&present);
            // Each held field's values over the span's entities.
            let mut held: Vec<Vec<Option<Number>>> = Vec::with_capacity(fields.len());
            for field in fields {
                let mut values = Vec::new();
                if let TallySource::Held(over) = &field.source {
                    values = vec![None; width];
                    over(&present, &mut |entity, value| {
                        values[(entity - start) as usize] = Some(value);
                    });
                }
                held.push(values);
            }
            // Each entity's key list, as a span of one flat buffer.
            let mut keys: Vec<u32> = Vec::new();
            let mut list_of: Vec<(u32, u32)> = vec![(0, 0); width];
            lists(span.clone(), &mut |entity, list| {
                let at = (entity - start) as usize;
                if rows[at] != u32::MAX {
                    list_of[at] = (keys.len() as u32, list.len() as u32);
                    keys.extend_from_slice(list);
                }
            })?;
            let mut index: HashMap<Vec<u32>, usize> = HashMap::new();
            let mut tallying: Vec<Vec<Tallying>> = Vec::new();
            let mut last: Option<(Range<usize>, usize)> = None;
            for entity in present.iter() {
                let at = (entity - start) as usize;
                let row = rows[at];
                let (from, len) = list_of[at];
                let list = from as usize..(from + len) as usize;
                // Neighbouring entities mostly carry the same key list, so an entity whose keys
                // are the last one's takes its tallies without a lookup.
                let group = match &last {
                    Some((held_list, group)) if keys[held_list.clone()] == keys[list.clone()] => {
                        *group
                    }
                    _ => {
                        let group = match index.get(&keys[list.clone()]) {
                            Some(&group) => group,
                            None => {
                                tallying
                                    .push(fields.iter().map(|f| Tallying::new(f.float)).collect());
                                index.insert(keys[list.clone()].to_vec(), tallying.len() - 1);
                                tallying.len() - 1
                            }
                        };
                        last = Some((list, group));
                        group
                    }
                };
                let tallies = &mut tallying[group];
                for (k, field) in fields.iter().enumerate() {
                    let value = match (&field.source, &drawn[k]) {
                        (TallySource::Held(_), _) => held[k][at],
                        (TallySource::Drawn, Some((slice, presence))) => presence
                            .contains(row)
                            .then(|| slice.number_at(row as usize))
                            .flatten(),
                        (TallySource::Drawn, None) => None,
                    };
                    tallies[k].add(value, row);
                }
            }
            let mut tallying: Vec<Option<Vec<Tallying>>> = tallying.into_iter().map(Some).collect();
            Ok(index
                .into_iter()
                .map(|(keys, group)| {
                    let tallies = tallying[group].take().expect("one group per key list");
                    (keys, tallies.into_iter().map(Tallying::finish).collect())
                })
                .collect())
        })
        .try_reduce(Groups::new, |mut a, b| {
            for (keys, tallies) in b {
                match a.get_mut(&keys) {
                    Some(held) => {
                        for (held, tally) in held.iter_mut().zip(&tallies) {
                            *held = std::mem::take(held).merged(tally, RESERVE);
                        }
                    }
                    None => {
                        a.insert(keys, tallies);
                    }
                }
            }
            Ok(a)
        });
    let mut groups: Vec<(Vec<u32>, Vec<FieldTally>)> = tallied?.into_iter().collect();
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let (lists, tallies) = groups.into_iter().unzip();
    let out = FieldTallies {
        fields: fields.iter().map(|f| (f.name.clone(), f.float)).collect(),
        lists,
        tallies,
    };
    write(path, &out)?;
    Ok(out)
}

/// A held field's values over a set of entities, for a [`TallySource::Held`] field.
pub type HeldValues = Box<dyn Fn(&croaring::Bitmap, &mut dyn FnMut(u32, Number)) + Sync + Send>;

/// The fields a view's base is tallied over, from the bundle's declared scalars: every number and
/// timestamp column without a vocabulary, drawn where it is rendered and otherwise held where it
/// is indexed. A group-scoped field is not among them, and a request walks its base rows.
pub fn tallied_fields(declared: &[DeclaredScalar]) -> Vec<(String, bool, bool)> {
    declared
        .iter()
        .filter(|d| d.vocabulary.is_none() && (d.render || d.index))
        .filter(|d| {
            !matches!(
                d.arrow_type,
                ScalarType::Bool | ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text
            )
        })
        .map(|d| {
            let float = matches!(d.arrow_type, ScalarType::F32 | ScalarType::F64);
            (d.name.clone(), float, d.render)
        })
        .collect()
}

/// [`derive`] over one view's base as a build or a fold has just written it, into the view's own
/// directory: its segment `seg_id`'s columns, its permutation and the partition's entity terms.
/// The fields are [`tallied_fields`] of `declared`, a held one read through `held` where it opens
/// one.
pub fn derive_view(
    partition_dir: &Path,
    view: &str,
    seg_id: &str,
    base_rows: u32,
    declared: &[DeclaredScalar],
    held: &dyn Fn(&str) -> io::Result<Option<HeldValues>>,
) -> crate::error::Result<std::path::PathBuf> {
    let view_dir = crate::view_path(partition_dir, view);
    let path = view_dir.join(FIELD_TALLIES_FILE);
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| crate::error::StoreError::Io { path, source }
    };
    let mut opened: Vec<(String, bool, Option<HeldValues>)> = Vec::new();
    for (name, float, drawn) in tallied_fields(declared) {
        match drawn {
            true => opened.push((name, float, None)),
            false => {
                if let Some(values) = held(&name).map_err(io_error(&path))? {
                    opened.push((name, float, Some(values)));
                }
            }
        }
    }
    let fields: Vec<TallyField<'_>> = opened
        .iter()
        .map(|(name, float, values)| TallyField {
            name: name.clone(),
            float: *float,
            source: match values {
                Some(values) => TallySource::Held(values.as_ref()),
                None => TallySource::Drawn,
            },
        })
        .collect();
    if base_rows == 0 {
        write(
            &path,
            &FieldTallies {
                fields: fields.iter().map(|f| (f.name.clone(), f.float)).collect(),
                ..FieldTallies::default()
            },
        )
        .map_err(io_error(&path))?;
        return Ok(path);
    }
    let columns = ColumnsRef::load(&view_dir.join("segments").join(seg_id).join("columns.arrow"))?;
    let permutation = crate::Permutation::load(&view_dir.join("permutation.bin"))?;
    let terms =
        crate::entity_terms::EntityTerms::open_dir(&partition_dir.join(crate::ENTITY_TERMS_DIR))?;
    let row_of = |entity: u32| {
        permutation
            .row_of(tessera_types::EntityId::new(u64::from(entity)))
            .map(|row| row.raw())
            .filter(|&row| row < base_rows)
    };
    let lists = |span: Range<u32>, f: &mut dyn FnMut(u32, &[u32])| {
        terms
            .for_each_in(span, f)
            .map_err(|e| io::Error::other(e.to_string()))
    };
    derive(
        permutation.bound(),
        Some(&columns),
        &row_of,
        &lists,
        &fields,
        &path,
    )
    .map_err(io_error(&path))?;
    Ok(path)
}

fn put_number(bytes: &mut Vec<u8>, value: Number) {
    let bits = match value {
        Number::Int(i) => i as u128,
        Number::Float(f) => u128::from(f.to_bits()),
    };
    bytes.extend_from_slice(&bits.to_le_bytes());
}

/// Write `tallies` to `path` and sync it.
pub fn write(path: &Path, tallies: &FieldTallies) -> io::Result<()> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(tallies.fields.len() as u32).to_le_bytes());
    for (name, float) in &tallies.fields {
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(u8::from(*float));
    }
    bytes.extend_from_slice(&(tallies.lists.len() as u32).to_le_bytes());
    for keys in &tallies.lists {
        bytes.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for key in keys {
            bytes.extend_from_slice(&key.to_le_bytes());
        }
    }
    for per_list in &tallies.tallies {
        for (tally, (_, float)) in per_list.iter().zip(&tallies.fields) {
            for n in [tally.rows, tally.none, tally.count] {
                bytes.extend_from_slice(&n.to_le_bytes());
            }
            let compact = match (&tally.sum, float) {
                (Sum::Int(i), false) => {
                    bytes.extend_from_slice(&i.to_le_bytes());
                    None
                }
                (Sum::Float(c), true) => Some(c.clone()),
                (sum, _) => Some(sum.exact().compact()),
            };
            if let Some(compact) = compact {
                for magnitude in [&compact.positive, &compact.negative] {
                    bytes.push(magnitude.first);
                    bytes.push(magnitude.words.len() as u8);
                    for word in magnitude.words.iter() {
                        bytes.extend_from_slice(&word.to_le_bytes());
                    }
                }
            }
            for side in [&tally.low, &tally.high] {
                bytes.push(side.len() as u8);
                for &(value, row) in side {
                    put_number(&mut bytes, value);
                    bytes.extend_from_slice(&row.to_le_bytes());
                }
            }
        }
    }
    let mut file = std::fs::File::create(path)?;
    io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()
}

/// Little-endian reads off the front of the file.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.take(16)?.try_into().ok()?))
    }
    fn number(&mut self, float: bool) -> Option<Number> {
        let bits = self.u128()?;
        Some(match float {
            true => Number::Float(f64::from_bits(bits as u64)),
            false => Number::Int(bits as i128),
        })
    }
}

/// The tallies in `bytes`, or `None` where they do not decode to the end.
fn decode(bytes: &[u8]) -> Option<FieldTallies> {
    let mut r = Reader(bytes);
    if r.take(8)? != MAGIC {
        return None;
    }
    let mut fields = Vec::new();
    for _ in 0..r.u32()? {
        let len = usize::from(r.u16()?);
        let name = String::from_utf8(r.take(len)?.to_vec()).ok()?;
        fields.push((name, r.u8()? != 0));
    }
    let mut lists = Vec::new();
    for _ in 0..r.u32()? {
        let n = r.u32()? as usize;
        let keys = (0..n).map(|_| r.u32()).collect::<Option<Vec<u32>>>()?;
        lists.push(keys);
    }
    let mut tallies = Vec::with_capacity(lists.len());
    for _ in 0..lists.len() {
        let mut per_list = Vec::with_capacity(fields.len());
        for &(_, float) in &fields {
            let (rows, none, count) = (r.u64()?, r.u64()?, r.u64()?);
            let sum = match float {
                false => Sum::Int(r.u128()? as i128),
                true => {
                    let mut magnitude = || -> Option<Trimmed> {
                        let first = r.u8()?;
                        let len = usize::from(r.u8()?);
                        if !CompactSum::fits(first, len) {
                            return None;
                        }
                        let words = (0..len).map(|_| r.u64()).collect::<Option<Vec<u64>>>()?;
                        Some(Trimmed {
                            first,
                            words: words.into(),
                        })
                    };
                    let positive = magnitude()?;
                    let negative = magnitude()?;
                    Sum::Float(CompactSum { positive, negative })
                }
            };
            let mut sides = [Vec::new(), Vec::new()];
            for side in &mut sides {
                for _ in 0..r.u8()? {
                    let value = r.number(float)?;
                    side.push((value, r.u32()?));
                }
            }
            let [low, high] = sides;
            per_list.push(FieldTally {
                rows,
                none,
                count,
                sum,
                low,
                high,
            });
        }
        tallies.push(per_list);
    }
    r.0.is_empty().then_some(FieldTallies {
        fields,
        lists,
        tallies,
    })
}

/// The tallies at `path`. An absent file is `Ok(None)`; one that does not decode is an error.
pub fn read(path: &Path) -> crate::error::Result<Option<FieldTallies>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(crate::error::StoreError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    decode(&bytes)
        .map(Some)
        .ok_or_else(|| crate::error::StoreError::MalformedBundle {
            detail: format!(
                "{} does not decode as a view's field tallies; rebuild the bundle",
                path.display()
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is written is what is read back, an integer field's sum and a float field's alike,
    /// and an entity with a row and no key list is tallied under the empty list.
    #[test]
    fn a_written_file_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FIELD_TALLIES_FILE);
        // Entities 0 to 5 have a row each, the entity's own number; 6 and 7 have none.
        let row_of = |entity: u32| (entity < 6).then_some(entity);
        // Entities 0 to 4 carry a list; 5 carries none.
        let lists = |span: Range<u32>, f: &mut dyn FnMut(u32, &[u32])| {
            for entity in span.filter(|&e| e < 5) {
                f(entity, &[entity % 2, 7]);
            }
            Ok(())
        };
        fn over(
            numbers: fn(u32) -> Option<Number>,
        ) -> impl Fn(&croaring::Bitmap, &mut dyn FnMut(u32, Number)) + Sync {
            move |entities, f| {
                for entity in entities.iter() {
                    if let Some(value) = numbers(entity) {
                        f(entity, value);
                    }
                }
            }
        }
        let score = over(|e| {
            [1.5, -2.0, f64::NAN, 1e300]
                .get(e as usize)
                .map(|&v| Number::Float(v))
        });
        let rank = over(|e| (e < 5).then_some(Number::Int(-i128::from(e))));
        let fields = [
            TallyField {
                name: "score".to_string(),
                float: true,
                source: TallySource::Held(&score),
            },
            TallyField {
                name: "rank".to_string(),
                float: false,
                source: TallySource::Held(&rank),
            },
        ];
        let derived = derive(8, None, &row_of, &lists, &fields, &path).unwrap();
        assert_eq!(derived.lists, vec![vec![], vec![0, 7], vec![1, 7]]);
        let unlisted = &derived.tallies[0][0];
        assert_eq!((unlisted.rows, unlisted.none), (1, 1), "entity 5");
        let even = &derived.tallies[1][0];
        assert_eq!(
            (even.rows, even.none, even.count),
            (3, 1, 1),
            "entities 0, 2 and 4: a value, a NaN and none"
        );
        assert_eq!(even.sum.mean(even.count), Some(1.5));
        let odd = &derived.tallies[2][0];
        assert_eq!((odd.none, odd.count), (0, 2));
        assert_eq!(odd.low.first(), Some(&(Number::Float(-2.0), 1)));
        assert_eq!(
            derived.tallies[1][1].sum,
            Sum::Int(-6),
            "entities 0, 2 and 4"
        );
        assert_eq!(read(&path).unwrap(), Some(derived));
        assert_eq!(read(&dir.path().join("absent")).unwrap(), None);
    }
}
