//! The tiler: sorts a batch of items into Morton/row order (contracts §2.6, R3).
//!
//! One implementation shared by `tessera build` now and streaming flush later (plan §5).
//! Deliberately free of I/O — segment writing lives in `tessera-store`. Priority is **not**
//! computed here: the entity-ID allocator owns priority assignment (R3, splitmix64 over the
//! final entity ID); callers pass it in already computed.

use tessera_types::EntityId;

use crate::morton::{morton_of, Extent};

/// A declared-scalar value carried alongside the fixed columns (`entity_id`, `x`, `y`,
/// `node_id`, `priority`). Phase 1 supports the three scalar kinds below (R4).
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    U64(u64),
    F32(f32),
    Utf8(String),
}

/// The Arrow type of a declared scalar column, used to build `columns.arrow`'s schema
/// (`scalar_schema` in [`crate::tiler`]'s consumers, e.g. `tessera_store::write::write_segment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarType {
    U64,
    F32,
    Utf8,
}

/// One item to be placed into a segment: its entity identity, geometry, node attachment,
/// priority (R3, computed by the caller), and any declared scalars.
#[derive(Debug, Clone, PartialEq)]
pub struct TilerItem {
    pub entity_id: EntityId,
    pub x: f32,
    pub y: f32,
    pub node_id: u32,
    pub priority: u16,
    pub scalars: Vec<ScalarValue>,
}

/// Sort `items` into segment (row) order: `(morton, priority, entity_id)` ascending
/// (contracts §2.6 — the priority tiebreak within equal Morton codes is contract, not
/// incidental). Row ID after sorting is simply the item's index.
///
/// Returns the sorted items' Morton codes as low-aligned `u64`s (the 32-bit code widened,
/// matching `morton.u64`'s on-disk representation), in the same order as `items` post-sort.
///
/// Morton codes are computed from the `f32` `x`/`y` values promoted to `f64` for the
/// quantisation math (R2: `cell()` is defined over `f64`), against `extent`.
pub fn sort_batch(items: &mut [TilerItem], extent: &Extent) -> Vec<u64> {
    // Pair each item with its Morton code up front so the sort comparator and the
    // returned code vector both derive from one computation (avoids recomputing per
    // comparison, and avoids the code and the sorted item order ever disagreeing).
    let mut codes: Vec<u64> = items
        .iter()
        .map(|item| morton_of(item.x as f64, item.y as f64, extent).raw() as u64)
        .collect();

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        codes[a]
            .cmp(&codes[b])
            .then(items[a].priority.cmp(&items[b].priority))
            .then(items[a].entity_id.raw().cmp(&items[b].entity_id.raw()))
    });

    // Apply the permutation to both `items` and `codes` in lockstep so the returned codes
    // stay aligned with `items`'s new order.
    let mut sorted_items = Vec::with_capacity(items.len());
    let mut sorted_codes = Vec::with_capacity(items.len());
    for &i in &order {
        sorted_items.push(items[i].clone());
        sorted_codes.push(codes[i]);
    }
    items.clone_from_slice(&sorted_items);
    codes = sorted_codes;
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_extent() -> Extent {
        Extent {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        }
    }

    fn item(entity_id: u64, x: f32, y: f32, priority: u16) -> TilerItem {
        TilerItem {
            entity_id: EntityId::new(entity_id),
            x,
            y,
            node_id: 0,
            priority,
            scalars: vec![],
        }
    }

    #[test]
    fn sorts_by_morton_then_priority_then_entity_id() {
        let e = unit_extent();
        // Two items at the identical coordinate (same Morton code): must order by
        // (priority, entity_id) — the tiebreak is contract (contracts §2.6).
        let mut items = vec![
            item(9, 0.5, 0.5, 5),
            item(2, 0.5, 0.5, 5),
            item(1, 0.5, 0.5, 1),
        ];
        let codes = sort_batch(&mut items, &e);
        assert_eq!(
            items.iter().map(|i| i.entity_id.raw()).collect::<Vec<_>>(),
            vec![1, 2, 9]
        );
        assert_eq!(codes.len(), 3);
        assert_eq!(codes[0], codes[1]);
        assert_eq!(codes[1], codes[2]);
    }

    #[test]
    fn returned_codes_are_non_decreasing() {
        let e = unit_extent();
        let mut items = vec![
            item(1, 0.9, 0.9, 0),
            item(2, 0.1, 0.1, 0),
            item(3, 0.5, 0.5, 0),
        ];
        let codes = sort_batch(&mut items, &e);
        assert!(codes.windows(2).all(|w| w[0] <= w[1]));
    }
}
