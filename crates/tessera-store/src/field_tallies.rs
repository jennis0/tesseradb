//! `field-tallies.bin` — per view, the figures of every number and timestamp field over the base
//! rows, one tally for each distinct list of index keys the rows' items carry.
//!
//! A viewer's visible base rows are the rows whose item carries a key the viewer's grant satisfies,
//! and items carrying the same key list are seen or not seen together. So a grant's figures over
//! the base are the merge of the tallies of the key lists it satisfies, and no request walks a base
//! row to find them. A build writes the file for each view it writes a base for, and so does a
//! fold, through [`derive`] both times.
//!
//! A tally holds the rows with no value, the count of finite values, their exact sum
//! ([`ExactSum`]), and the [`RESERVE`] smallest and largest finite values with their rows, so a
//! request that subtracts denied rows can still name its extremes.
//!
//! # The format
//!
//! Little-endian, after the magic `TSFT0001`:
//!
//! ```text
//! fields   u32 F, then per field: name length u16, name, float u8
//! lists    u32 L, then per list: key count u32, keys u32 ascending
//! tallies  L x F, list-major: none u64, count u64, sum (two magnitudes, each: first word u8,
//!          word count u8, words u64), low count u8, (value 16 B, row u32) each, high likewise
//! ```
//!
//! A value is an `i128`, or an `f64`'s bits in the low eight bytes, as its field's float byte
//! says. The file is listed in the manifest's digests, and a file that does not decode to its end
//! is refused.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use rayon::prelude::*;
use tessera_types::scalar::{Number, ScalarType};

pub use crate::exact_sum::ExactSum;
use crate::read::ColumnsRef;
use crate::render_presence::RenderPresence;

/// The file's name in a view's directory.
pub const FIELD_TALLIES_FILE: &str = "field-tallies.bin";

const MAGIC: &[u8; 8] = b"TSFT0001";

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

/// One set of rows' values of a field: how many rows hold no value, and the count and exact sum of
/// the finite values, with the most extreme of them, each with its row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FieldTally {
    pub none: u64,
    pub count: u64,
    pub sum: ExactSum,
    /// The smallest values, ascending.
    pub low: Vec<(Number, u32)>,
    /// The largest values, descending.
    pub high: Vec<(Number, u32)>,
}

impl FieldTally {
    /// Take one row's value, keeping `keep` extremes on each side.
    #[inline]
    pub fn add(&mut self, value: Option<Number>, row: u32, keep: usize) {
        match value {
            None => self.none += 1,
            Some(Number::Float(f)) if !f.is_finite() => {}
            Some(value) => {
                self.count += 1;
                match value {
                    Number::Int(i) => self.sum.add_int(i),
                    Number::Float(f) => self.sum.add_float(f),
                }
                if keep > 0 {
                    keep_extreme(&mut self.low, keep, (value, row), false);
                    keep_extreme(&mut self.high, keep, (value, row), true);
                }
            }
        }
    }

    /// Both tallies, keeping `keep` values on each side.
    pub fn merged(mut self, other: &FieldTally, keep: usize) -> FieldTally {
        self.none += other.none;
        self.count += other.count;
        self.sum = self.sum.plus(&other.sum);
        for &value in &other.low {
            keep_extreme(&mut self.low, keep, value, false);
        }
        for &value in &other.high {
            keep_extreme(&mut self.high, keep, value, true);
        }
        self
    }
}

/// One field's values over the base rows, for [`derive`].
pub enum TallySource<'a> {
    /// A drawn column of the base segment's `columns.arrow`, read by row.
    Drawn,
    /// Values held per entity, `None` where the entity holds none.
    Held(&'a (dyn Fn(u32) -> Option<Number> + Sync)),
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

/// Rows of the base one piece of [`derive`] reads.
const PIECE_ROWS: u32 = 1 << 20;

/// Tally `fields` over the `base_rows` rows of a view's base segment, whose `columns.arrow` is
/// `columns`, grouping each row by its item's key list, and write the result to `path`.
///
/// `entity_of_row` names each base row's entity, and `keys_of` an entity's sorted index keys, as
/// the postings list it under.
pub fn derive(
    base_rows: u32,
    columns: Option<&ColumnsRef>,
    entity_of_row: &(dyn Fn(u32) -> u32 + Sync),
    keys_of: &(dyn Fn(u32) -> io::Result<Vec<u32>> + Sync),
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
    let pieces: Vec<u32> = (0..base_rows.div_ceil(PIECE_ROWS)).collect();
    let tallied: io::Result<Groups> = pieces
        .into_par_iter()
        .map(|piece| -> io::Result<Groups> {
            // Rows near each other in map order mostly carry the same key list, so the last one's
            // tallies are taken without a lookup.
            let mut index: HashMap<Vec<u32>, usize> = HashMap::new();
            let mut lists: Vec<Vec<FieldTally>> = Vec::new();
            let mut last: Option<(Vec<u32>, usize)> = None;
            let end = piece
                .saturating_add(1)
                .saturating_mul(PIECE_ROWS)
                .min(base_rows);
            for row in piece * PIECE_ROWS..end {
                let entity = entity_of_row(row);
                let keys = keys_of(entity)?;
                let at = match &last {
                    Some((held, at)) if *held == keys => *at,
                    _ => {
                        let at = *index.entry(keys.clone()).or_insert_with(|| {
                            lists.push(vec![FieldTally::default(); fields.len()]);
                            lists.len() - 1
                        });
                        last = Some((keys, at));
                        at
                    }
                };
                let tallies = &mut lists[at];
                for (k, field) in fields.iter().enumerate() {
                    let value = match (&field.source, &drawn[k]) {
                        (TallySource::Held(value_of), _) => value_of(entity),
                        (TallySource::Drawn, Some((slice, presence))) => presence
                            .contains(row)
                            .then(|| slice.value_at(row as usize))
                            .flatten()
                            .and_then(|value| value.number()),
                        (TallySource::Drawn, None) => None,
                    };
                    tallies[k].add(value, row, RESERVE);
                }
            }
            let mut lists: Vec<Option<Vec<FieldTally>>> = lists.into_iter().map(Some).collect();
            Ok(index
                .into_iter()
                .map(|(keys, at)| (keys, lists[at].take().expect("one list per key list")))
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

/// Values held per entity, for a [`TallySource::Held`] field.
pub type HeldValues = Box<dyn Fn(u32) -> Option<Number> + Sync + Send>;

/// [`derive`] over one view's base as a build or a fold has just written it, into the view's own
/// directory: its segment `seg_id`'s columns, its row-to-entity table and the partition's entity
/// terms. The fields are every number and timestamp column of `drawn`, the segment's scalar tail,
/// and of `indexed`, the indexed columns that are not drawn, each read through `held` where it
/// opens one.
pub fn derive_view(
    partition_dir: &Path,
    view: &str,
    seg_id: &str,
    base_rows: u32,
    drawn: &[(String, ScalarType)],
    indexed: &[(String, ScalarType)],
    held: &dyn Fn(&str) -> io::Result<Option<HeldValues>>,
) -> crate::error::Result<std::path::PathBuf> {
    let view_dir = crate::view_path(partition_dir, view);
    let path = view_dir.join(FIELD_TALLIES_FILE);
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| crate::error::StoreError::Io { path, source }
    };
    let numeric = |ty: &ScalarType| {
        !matches!(
            ty,
            ScalarType::Bool | ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text
        )
    };
    let float = |ty: &ScalarType| matches!(ty, ScalarType::F32 | ScalarType::F64);
    let mut opened: Vec<(String, bool, Option<HeldValues>)> = Vec::new();
    for (name, ty) in drawn.iter().filter(|(_, ty)| numeric(ty)) {
        opened.push((name.clone(), float(ty), None));
    }
    for (name, ty) in indexed.iter().filter(|(_, ty)| numeric(ty)) {
        if let Some(values) = held(name).map_err(io_error(&path))? {
            opened.push((name.clone(), float(ty), Some(values)));
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
    let columns = match base_rows {
        0 => None,
        _ => Some(ColumnsRef::load(
            &view_dir.join("segments").join(seg_id).join("columns.arrow"),
        )?),
    };
    let rows = match base_rows {
        0 => None,
        _ => Some(crate::RowToEntity::load(
            &view_dir.join(crate::ROW_ENTITY_FILE),
        )?),
    };
    let terms =
        crate::entity_terms::EntityTerms::open_dir(&partition_dir.join(crate::ENTITY_TERMS_DIR))?;
    let entity_of_row = |row: u32| {
        rows.as_ref()
            .and_then(|rows| rows.entity_of(tessera_types::RowId::new(row)))
            .map_or(u32::MAX, |e| e.raw() as u32)
    };
    let keys_of = |entity: u32| {
        terms
            .terms_of(entity)
            .map(Option::unwrap_or_default)
            .map_err(|e| io::Error::other(e.to_string()))
    };
    derive(
        base_rows,
        columns.as_ref(),
        &entity_of_row,
        &keys_of,
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
        for tally in per_list {
            bytes.extend_from_slice(&tally.none.to_le_bytes());
            bytes.extend_from_slice(&tally.count.to_le_bytes());
            let words: Vec<u64> = tally.sum.words().collect();
            for magnitude in words.chunks(ExactSum::WORDS / 2) {
                let first = magnitude.iter().position(|&w| w != 0).unwrap_or(0);
                let last = magnitude.iter().rposition(|&w| w != 0).map_or(0, |l| l + 1);
                let held = &magnitude[first..last.max(first)];
                bytes.push(first as u8);
                bytes.push(held.len() as u8);
                for word in held {
                    bytes.extend_from_slice(&word.to_le_bytes());
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
    fn number(&mut self, float: bool) -> Option<Number> {
        let bits = u128::from_le_bytes(self.take(16)?.try_into().ok()?);
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
            let none = r.u64()?;
            let count = r.u64()?;
            let mut words = vec![0u64; ExactSum::WORDS];
            for magnitude in words.chunks_mut(ExactSum::WORDS / 2) {
                let first = usize::from(r.u8()?);
                let held = usize::from(r.u8()?);
                for at in first..first.checked_add(held)? {
                    *magnitude.get_mut(at)? = r.u64()?;
                }
            }
            let sum = ExactSum::of_words(&words)?;
            let mut sides = [Vec::new(), Vec::new()];
            for side in &mut sides {
                for _ in 0..r.u8()? {
                    let value = r.number(float)?;
                    side.push((value, r.u32()?));
                }
            }
            let [low, high] = sides;
            per_list.push(FieldTally {
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

    /// What is written is what is read back.
    #[test]
    fn a_written_file_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FIELD_TALLIES_FILE);
        let values: Vec<f64> = vec![1.5, -2.0, f64::NAN, 1e300];
        let entity = |row: u32| row;
        let keys = |entity: u32| Ok(vec![entity % 2, 7]);
        let held = |entity: u32| values.get(entity as usize).map(|&v| Number::Float(v));
        let fields = [TallyField {
            name: "score".to_string(),
            float: true,
            source: TallySource::Held(&held),
        }];
        let derived = derive(6, None, &entity, &keys, &fields, &path).unwrap();
        assert_eq!(derived.lists, vec![vec![0, 7], vec![1, 7]]);
        let even = &derived.tallies[0][0];
        assert_eq!(
            (even.none, even.count),
            (1, 1),
            "rows 0, 2 and 4: a value, a NaN and none"
        );
        assert_eq!(even.sum.mean(even.count), Some(1.5));
        let odd = &derived.tallies[1][0];
        assert_eq!((odd.none, odd.count), (1, 2));
        assert_eq!(odd.low.first(), Some(&(Number::Float(-2.0), 1)));
        assert_eq!(read(&path).unwrap(), Some(derived));
        assert_eq!(read(&dir.path().join("absent")).unwrap(), None);
    }
}
