//! The rule applied to a build's files by [`resolve`], one file at a time, over maps of what the
//! files read before hold: the linear build's numbering and the streaming build's reference.

use rustc_hash::FxHashMap;
use tessera_lifecycle::resolve::{self, Batch, Holdings, RowIdentity, Verdict};
use tessera_types::{EntityId, TesseraId};

use super::report::Tally;
use super::scan::FileRead;
use super::{Limit, Numbering, Numbers, Read, ReadInput, ReadRows};
use crate::error::{BuildError, Result};
use crate::row_groups::FileGroups;
use crate::spill::mix64;

/// Who holds each value of each unique field, and each item's value of each field.
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
    ) -> std::result::Result<Vec<Vec<EntityId>>, Self::Error> {
        Ok(keys
            .iter()
            .map(|key| {
                self.holder
                    .get(&(field, *key))
                    .map(|&item| vec![EntityId::new(u64::from(item))])
                    .unwrap_or_default()
            })
            .collect())
    }

    /// Nothing holds a `tessera_id` in the empty database a build starts from.
    fn tessera_holders(
        &self,
        ids: &[TesseraId],
    ) -> std::result::Result<Vec<Option<EntityId>>, Self::Error> {
        Ok(vec![None; ids.len()])
    }
}

/// Number every file of the build.
pub(crate) fn number(args: &crate::BuildArgs) -> Result<Numbering> {
    let limit = Limit::of(&args.schema, args.limit)?;
    let mut held = Held::default();
    let mut next = 0u64;
    let mut numbering = Numbering {
        reads: Vec::new(),
        items: 0,
        refused: Vec::new(),
    };
    for read in super::reads(args)? {
        let (identities, texts): (Vec<Option<RowIdentity>>, Option<Vec<String>>) =
            match &read.input {
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
                    let file = FileRead::new(path, &groups, &carried, select.as_ref(), limit.as_ref());
                    if read.batch != Batch::Creates {
                        resolve::require_identifier(file.tessera, carried.len()).map_err(|_| {
                            BuildError::Invalid(super::no_identifier(&read.object, path))
                        })?;
                    }
                    let mut identities = vec![None; groups.rows() as usize];
                    file.scan(|scanned| {
                        for offset in 0..scanned.len {
                            if !scanned.selected.as_ref().is_none_or(|s| s[offset]) {
                                continue;
                            }
                            let carries_tessera =
                                scanned.tessera.as_ref().is_some_and(|t| t[offset]);
                            identities[scanned.first as usize + offset] = Some(RowIdentity {
                                tessera_id: carries_tessera.then(|| TesseraId::new(0)),
                                unique: carried
                                    .iter()
                                    .zip(&scanned.keys)
                                    .filter_map(|(field, keys)| {
                                        keys[offset].map(|key| (field.position, key.widen()))
                                    })
                                    .collect(),
                            });
                        }
                        Ok(())
                    })?;
                    (identities, None)
                }
                ReadInput::Lists(lists) => (
                    (0..lists.len())
                        .map(|member| {
                            Some(RowIdentity {
                                tessera_id: None,
                                unique: lists
                                    .carried
                                    .iter()
                                    .zip(&lists.keys)
                                    .filter_map(|(field, keys)| {
                                        keys[member].map(|key| (field.position, key.widen()))
                                    })
                                    .collect(),
                            })
                        })
                        .collect(),
                    Some(lists.texts.clone()),
                ),
            };
        let (rows, tally) = decide(&read, &identities, &mut held, &mut next)?;
        let refused = match (&read.input, texts) {
            (_, Some(texts)) => {
                tally.finish(&read.source, &read.object, |row| texts[row as usize].clone())
            }
            (ReadInput::File { path, fields, select }, None) => {
                let groups = FileGroups::open(path)?;
                let carried = super::carried_unique(groups.schema(), fields, &args.schema)
                    .map_err(|detail| BuildError::Schema {
                        path: path.clone(),
                        detail,
                    })?;
                let file = FileRead::new(path, &groups, &carried, select.as_ref(), limit.as_ref());
                let values = file.values_at(&tally.sampled())?;
                tally.finish(&read.source, &read.object, |row| {
                    values.get(&row).cloned().unwrap_or_else(|| format!("row {row}"))
                })
            }
            (ReadInput::Lists(_), None) => unreachable!("a list read carries its texts"),
        };
        if args.strict && !refused.is_empty() {
            return Err(super::stream::strict_refusal(&refused));
        }
        numbering.refused.extend(refused);
        numbering.reads.push((read.kind, rows));
    }
    numbering.items = next;
    Ok(numbering)
}

/// One file's rows decided by the rule: the selected rows resolved as one batch against what the
/// earlier files hold, the new items numbered in row order, and the values set taken into the
/// holdings.
fn decide(
    read: &Read,
    identities: &[Option<RowIdentity>],
    held: &mut Held,
    next: &mut u64,
) -> Result<(ReadRows, Tally)> {
    let rows: Vec<usize> = (0..identities.len())
        .filter(|&row| identities[row].is_some())
        .collect();
    let batch: Vec<RowIdentity> = rows
        .iter()
        .map(|&row| identities[row].clone().expect("selected"))
        .collect();
    let verdicts = match resolve::resolve(&batch, held, read.batch) {
        Ok(verdicts) => verdicts,
        Err(never) => match never {},
    };
    let mut numbers = vec![0u32; identities.len()];
    let mut tally = Tally::default();
    let mut named = 0u64;
    let mut mixed = 0u64;
    for ((&row, verdict), identity) in rows.iter().zip(&verdicts).zip(&batch) {
        let item = match verdict {
            Verdict::Refused(refusal) => {
                tally.refuse_row(refusal, row as u64);
                continue;
            }
            Verdict::Names(entity) => entity.raw() as u32,
            Verdict::Creates => {
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
