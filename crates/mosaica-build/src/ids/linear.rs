//! The rule applied to a build's files by [`resolve`], one file at a time, over maps of what the
//! files read before hold: the linear build's numbering and the streaming build's reference.

use rustc_hash::FxHashMap;
use mosaica_lifecycle::resolve::{self, Batch, Holdings, RowIdentity, Verdict};
use mosaica_types::{EntityId, ItemAt, MosaicaId, ShardId};

use super::report::{Tally, OUTSIDE_LIMIT};
use super::scan::FileRead;
use super::{check_cap, Limit, Limited, Numbering, Numbers, Read, ReadInput, ReadRows};
use crate::error::{BuildError, Result};
use crate::row_groups::FileGroups;
use crate::spill::mix64;

/// Who holds each value of each unique field, and each item's value of each field. The build
/// numbers items before it places them in shards, so it names each item in shard 0 by its number,
/// and refuses a corpus past one shard's size.
#[derive(Default)]
struct Held {
    holder: FxHashMap<(u16, u128), u32>,
    value: FxHashMap<(u32, u16), u128>,
}

impl Holdings for Held {
    type Error = std::convert::Infallible;

    fn holders(
        &self,
        field: u16,
        keys: &[resolve::Key],
    ) -> std::result::Result<Vec<(usize, ItemAt)>, Self::Error> {
        Ok(keys
            .iter()
            .enumerate()
            .filter_map(|(at, key)| {
                let item = self.holder.get(&(field, *key))?;
                Some((
                    at,
                    ItemAt::new(ShardId::ZERO, EntityId::new(u64::from(*item))),
                ))
            })
            .collect())
    }

    /// Nothing holds a `mosaica_id` in the empty database a build starts from.
    fn mosaica_holders(
        &self,
        ids: &[MosaicaId],
    ) -> std::result::Result<Vec<Option<ItemAt>>, Self::Error> {
        Ok(vec![None; ids.len()])
    }
}

/// One row the rule reads, and whether `--limit` leaves it out unless it names an item.
type Row = (RowIdentity, bool);

/// Number every file of the build.
pub(crate) fn number(args: &crate::BuildArgs) -> Result<Numbering> {
    let mut limited = Limit::of(&args.schema, args.limit)?.map(Limited::new);
    let mut held = Held::default();
    let mut next = 0u64;
    let mut numbering = Numbering {
        reads: Vec::new(),
        items: 0,
        refused: Vec::new(),
    };
    for read in super::reads(args)? {
        let mut outside = Tally::default();
        let (rows, texts): (Vec<Option<Row>>, Option<Vec<String>>) = match &read.input {
            ReadInput::File {
                path,
                fields,
                select,
            } => {
                let groups = FileGroups::open(path)?;
                let carried = super::carried_unique(groups.schema(), fields, &args.schema)
                    .map_err(|detail| BuildError::Schema {
                        path: path.clone(),
                        detail,
                    })?;
                let creates = read.batch == Batch::Creates;
                let limit_read = limited.as_ref().and_then(|l| l.read(&carried, creates));
                let file = FileRead::new(path, &groups, &carried, select.as_ref(), limit_read);
                if !creates {
                    resolve::require_identifier(file.mosaica, carried.len()).map_err(|_| {
                        BuildError::Invalid(super::no_identifier(&read.object, path))
                    })?;
                }
                let mut rows = vec![None; groups.rows() as usize];
                file.scan(|scanned| {
                    for offset in 0..scanned.len {
                        if !scanned.selected.as_ref().is_none_or(|s| s[offset]) {
                            if scanned.outside.as_ref().is_some_and(|o| o[offset]) {
                                outside.refuse(OUTSIDE_LIMIT, scanned.first + offset as u64);
                            }
                            continue;
                        }
                        let carries_mosaica = scanned.mosaica.as_ref().is_some_and(|t| t[offset]);
                        let identity = RowIdentity {
                            mosaica_id: carries_mosaica.then(|| MosaicaId::new(0)),
                            unique: carried
                                .iter()
                                .zip(&scanned.keys)
                                .filter_map(|(field, keys)| {
                                    keys[offset].map(|key| (field.position, key.widen()))
                                })
                                .collect(),
                        };
                        let left_out = scanned.left_out.as_ref().is_some_and(|l| l[offset]);
                        rows[scanned.first as usize + offset] = Some((identity, left_out));
                    }
                    Ok(())
                })?;
                (rows, None)
            }
            ReadInput::Lists(lists) => {
                let scanned = Limited::lists(limited.as_ref(), lists);
                for (member, &out) in scanned.outside.iter().flatten().enumerate() {
                    if out {
                        outside.refuse(OUTSIDE_LIMIT, member as u64);
                    }
                }
                let rows = (0..lists.len())
                    .map(|member| {
                        if scanned.selected.as_ref().is_some_and(|s| !s[member]) {
                            return None;
                        }
                        let identity = RowIdentity {
                            mosaica_id: None,
                            unique: lists
                                .carried
                                .iter()
                                .zip(&lists.keys)
                                .filter_map(|(field, keys)| {
                                    keys[member].map(|key| (field.position, key.widen()))
                                })
                                .collect(),
                        };
                        let left_out = scanned.left_out.as_ref().is_some_and(|l| l[member]);
                        Some((identity, left_out))
                    })
                    .collect();
                (rows, Some(lists.texts.clone()))
            }
        };
        let (rows, mut tally) = decide(
            &read,
            &rows,
            &mut held,
            &mut next,
            limited.as_mut(),
            args.shard_size(),
        )?;
        tally.merge(outside);
        let refused = match (&read.input, texts) {
            (_, Some(texts)) => tally.finish(&read.source, &read.object, |row| {
                texts[row as usize].clone()
            }),
            (
                ReadInput::File {
                    path,
                    fields,
                    select,
                },
                None,
            ) => {
                let groups = FileGroups::open(path)?;
                let carried = super::carried_unique(groups.schema(), fields, &args.schema)
                    .map_err(|detail| BuildError::Schema {
                        path: path.clone(),
                        detail,
                    })?;
                let file = FileRead::new(path, &groups, &carried, select.as_ref(), None);
                let values = file.values_at(&tally.sampled())?;
                tally.finish(&read.source, &read.object, |row| {
                    values
                        .get(&row)
                        .cloned()
                        .unwrap_or_else(|| format!("row {row}"))
                })
            }
            (ReadInput::Lists(_), None) => unreachable!("a list read carries its texts"),
        };
        if args.strict && refused.iter().any(super::RefusedRows::is_refusal) {
            return Err(super::stream::strict_refusal(&refused));
        }
        numbering.refused.extend(refused);
        numbering.reads.push((read.kind, rows));
    }
    numbering.items = next;
    Ok(numbering)
}

/// One file's rows decided by the rule: the rows read resolved as one batch against what the
/// earlier files hold, the new items numbered in row order, and the values set taken into the
/// holdings. A row `--limit` leaves out unless it names an item, and which names none, is not part
/// of the batch.
fn decide(
    read: &Read,
    rows: &[Option<Row>],
    held: &mut Held,
    next: &mut u64,
    mut limited: Option<&mut Limited>,
    shard_size: u64,
) -> Result<(ReadRows, Tally)> {
    let mut tally = Tally::default();
    let mut positions: Vec<usize> = Vec::new();
    let mut batch: Vec<RowIdentity> = Vec::new();
    for (row, entry) in rows.iter().enumerate() {
        let Some((identity, left_out)) = entry else {
            continue;
        };
        let names = identity.mosaica_id.is_some()
            || identity
                .unique
                .iter()
                .any(|value| held.holder.contains_key(value));
        if *left_out && !names {
            if read.batch != Batch::Creates {
                tally.refuse(OUTSIDE_LIMIT, row as u64);
            }
            continue;
        }
        positions.push(row);
        batch.push(identity.clone());
    }
    let verdicts = match resolve::resolve(&batch, held, read.batch) {
        Ok(verdicts) => verdicts,
        Err(never) => match never {},
    };
    let mut numbers = vec![0u32; rows.len()];
    let mut named = 0u64;
    let mut mixed = 0u64;
    for ((&row, verdict), identity) in positions.iter().zip(&verdicts).zip(&batch) {
        let item = match verdict {
            Verdict::Refused(refusal) => {
                tally.refuse_row(refusal, row as u64);
                continue;
            }
            Verdict::Names(item) => item.entity.raw() as u32,
            Verdict::Creates => {
                check_one_shard(*next + 1, shard_size)?;
                let item = *next as u32;
                *next += 1;
                item
            }
        };
        numbers[row] = item + 1;
        named += 1;
        mixed = mixed.wrapping_add(mix64(u64::from(item)));
        if matches!(read.batch, Batch::Creates | Batch::Edits) {
            for &(field, key) in &identity.unique {
                if let Some(old) = held.value.insert((item, field), key) {
                    if old != key {
                        held.holder.remove(&(field, old));
                    }
                }
                held.holder.insert((field, key), item);
                if let Some(limited) = limited.as_deref_mut() {
                    limited.given(field);
                    if limited.raises(field, key) {
                        limited.raise();
                    }
                }
            }
        }
    }
    Ok((
        ReadRows {
            numbers: Numbers::Held(numbers),
            groups: None,
            named,
            mixed,
        },
        tally,
    ))
}

/// [`check_cap`], and past one shard of `size`: the linear build writes one shard.
pub(super) fn check_one_shard(items: u64, size: u64) -> Result<()> {
    check_cap(items)?;
    if items <= size {
        return Ok(());
    }
    Err(BuildError::Invalid(format!(
        "this build's files create at least {items} items, more than the shard size {size}, and \
         the linear build writes one shard. Build with `mosaica_build::build`, or declare \
         `[shards] size` of at least the number of items the files create"
    )))
}
