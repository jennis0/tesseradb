//! An aggregate cursor's sealed payload: the table in progress, the groups it lists, and the row
//! the next page starts after. Sealed and bound as the records cursors are, with a digest of the
//! request in the binding, so a cursor opens only for the request that issued it.

use sha2::{Digest, Sha256};

use super::{AggregateRequest, By, Pick, Reference};
use crate::error::{EngineError, Result};

/// How far a read has gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Position {
    /// The table in progress, or the number of tables once every one is sent.
    pub(super) table: u32,
    /// The groups the table in progress lists, fixed at its first page: codes, or
    /// `level << 32 | ordinal`. `None` before the table's first page.
    pub(super) chosen: Option<Vec<u64>>,
    /// The next group to send, as a position in `chosen` followed by the rest and none.
    pub(super) group: u32,
    /// The last cell of `group` sent, on a table with cells.
    pub(super) after_cell: Option<u64>,
    /// The segments and overlay versions the last page counted under.
    pub(super) stamp: Option<(u64, u64)>,
}

impl Position {
    pub(super) fn start() -> Position {
        Position {
            table: 0,
            chosen: None,
            group: 0,
            after_cell: None,
            stamp: None,
        }
    }

    /// The start of the table after this one.
    pub(super) fn next_table(&self) -> Position {
        Position {
            table: self.table + 1,
            chosen: None,
            group: 0,
            after_cell: None,
            stamp: self.stamp,
        }
    }
}

pub(super) struct AggregateCursor;

impl AggregateCursor {
    /// `table u32, group u32, flags u8 (1 chosen, 2 after_cell, 4 stamp), after_cell u64,
    /// stamp 2 × u64, chosen count u32, chosen u64 each`, little-endian.
    pub(super) fn encode(position: &Position) -> Vec<u8> {
        let chosen = position.chosen.as_deref().unwrap_or(&[]);
        let mut out = Vec::with_capacity(33 + 4 + 8 * chosen.len());
        out.extend_from_slice(&position.table.to_le_bytes());
        out.extend_from_slice(&position.group.to_le_bytes());
        let flags = u8::from(position.chosen.is_some())
            | u8::from(position.after_cell.is_some()) << 1
            | u8::from(position.stamp.is_some()) << 2;
        out.push(flags);
        out.extend_from_slice(&position.after_cell.unwrap_or(0).to_le_bytes());
        let (a, b) = position.stamp.unwrap_or((0, 0));
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
        out.extend_from_slice(&(chosen.len() as u32).to_le_bytes());
        for &group in chosen {
            out.extend_from_slice(&group.to_le_bytes());
        }
        out
    }

    /// A payload that opened and does not parse is refused like one that did not open.
    pub(super) fn decode(payload: &[u8]) -> Result<Position> {
        const FIXED: usize = 4 + 4 + 1 + 8 + 16 + 4;
        if payload.len() < FIXED || payload[8] > 7 {
            return Err(EngineError::CursorRefused);
        }
        let u32_at = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().expect("4"));
        let u64_at = |at: usize| u64::from_le_bytes(payload[at..at + 8].try_into().expect("8"));
        let flags = payload[8];
        let count = u32_at(33) as usize;
        if payload.len() != FIXED + 8 * count || (flags & 1 == 0 && count > 0) {
            return Err(EngineError::CursorRefused);
        }
        Ok(Position {
            table: u32_at(0),
            group: u32_at(4),
            chosen: (flags & 1 == 1)
                .then(|| (0..count).map(|i| u64_at(FIXED + 8 * i)).collect()),
            after_cell: (flags & 2 == 2).then(|| u64_at(9)),
            stamp: (flags & 4 == 4).then(|| (u64_at(17), u64_at(25))),
        })
    }
}

/// A digest of what a request asks: its set, its reference and its groupings. A cursor is bound
/// to it, so it continues only the request it was issued for.
pub(super) fn digest(req: &AggregateRequest<'_>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"tessera-aggregate-request-v1");
    let mut part = |text: String| {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    };
    part(format!("{:?}", req.filter));
    part(match &req.reference {
        None => "none".to_string(),
        Some(Reference::Visible) => "visible".to_string(),
        Some(Reference::Filter(expr)) => format!("{expr:?}"),
    });
    for grouping in req.groupings {
        let by = match &grouping.by {
            None => "-".to_string(),
            Some(By::Field { column, pick }) => format!("field {column:?} {}", pick_text(pick)),
            Some(By::Layer { layer, level, pick }) => {
                format!("layer {layer:?} {level:?} {}", pick_text(pick))
            }
        };
        part(format!("{by} cells {:?}", grouping.cells));
    }
    hasher.finalize().into()
}

fn pick_text<K: std::fmt::Debug>(pick: &Pick<K>) -> String {
    match pick {
        Pick::Top(n) => format!("top {n}"),
        Pick::Named(keys) => format!("named {keys:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_reads_back_as_it_was_written() {
        for position in [
            Position::start(),
            Position {
                table: 3,
                chosen: Some(vec![7, 1 << 40 | 9, 0]),
                group: 2,
                after_cell: Some(u64::MAX),
                stamp: Some((12, 40)),
            },
            Position {
                table: 1,
                chosen: Some(Vec::new()),
                group: 0,
                after_cell: None,
                stamp: Some((1, 2)),
            },
        ] {
            let encoded = AggregateCursor::encode(&position);
            assert_eq!(AggregateCursor::decode(&encoded).unwrap(), position);
            assert!(AggregateCursor::decode(&encoded[..encoded.len() - 1]).is_err());
        }
    }
}
