//! Per-session handle tables — the I10 trust boundary.
//!
//! `EntityId` never crosses to a viewer (design §4, I10): every point a session receives is
//! addressed by an opaque, per-session [`Handle`] instead. [`HandleTable`] is the sole place in
//! this crate — indeed, per the layer check (`scripts/check-layers.sh`), the sole place *outside*
//! this module in the whole crate — permitted to import `EntityId`; `crate::payload` accepts
//! `Handle` and plain columns exclusively.
//!
//! A caller (`tessera-server`, Task 13) holds one `HandleTable` per session, typically behind a
//! `parking_lot::Mutex` since a session's requests are not necessarily single-threaded; this
//! module has no opinion on that locking and exposes plain `&mut self` methods.
//!
//! **Sequential-mint leak rationale.** `handle_for` mints handles `0, 1, 2, …` in first-visit
//! order within a session. That order is not free of information: it discloses the sequence in
//! which distinct entities were first returned to *this* viewer across *their own* requests. But
//! the viewer already observes that order directly — it is exactly the order in which their own
//! viewport/pan/zoom calls surfaced those entities to them in the first place — so encoding it
//! into the handle assignment discloses nothing beyond what the viewer's own request history
//! already tells them. It does **not** disclose anything about *other* sessions (each table is
//! independent, see the isolation test below), nor about entity IDs, entity-ID density, or
//! insertion order in the underlying store. The one property this construction deliberately
//! defers is SA §4.5's per-session **keyed permutation** encoding, which additionally hides
//! within-session visit order from a viewer correlating handles across their own requests over
//! time (e.g. to infer whether two viewport calls re-surfaced the same entity without it being
//! obviously "the same handle again"); that hardening arrives with router/worker fan-out, not
//! Phase 1's single-process walking skeleton.

use std::collections::HashMap;

use tessera_types::{EntityId, Handle};

/// A per-session mapping between `EntityId` and per-session opaque `Handle`.
///
/// Handles are minted sequentially on first sight of an entity within *this* table and are
/// stable for the table's lifetime; a fresh `HandleTable` (e.g. a new session) starts its
/// numbering over, so the same entity gets an independent handle in each session (I10).
#[derive(Debug, Default)]
pub struct HandleTable {
    entity_to_handle: HashMap<EntityId, Handle>,
    handle_to_entity: Vec<EntityId>,
}

impl HandleTable {
    /// A fresh, empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The stable handle for `e` within this session, minting one on first sight.
    pub fn handle_for(&mut self, e: EntityId) -> Handle {
        if let Some(existing) = self.entity_to_handle.get(&e) {
            return *existing;
        }
        let handle = Handle::new(self.handle_to_entity.len() as u32);
        self.handle_to_entity.push(e);
        self.entity_to_handle.insert(e, handle);
        handle
    }

    /// The entity behind `h`, or `None` if this table never minted it.
    pub fn entity_of(&self, h: Handle) -> Option<EntityId> {
        self.handle_to_entity.get(h.raw() as usize).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_is_stable_within_a_session() {
        let mut table = HandleTable::new();
        let e = EntityId::new(42);
        let h1 = table.handle_for(e);
        let h2 = table.handle_for(e);
        assert_eq!(h1, h2);
        assert_eq!(table.entity_of(h1), Some(e));
    }

    #[test]
    fn handles_mint_sequentially_on_first_sight() {
        let mut table = HandleTable::new();
        let a = table.handle_for(EntityId::new(100));
        let b = table.handle_for(EntityId::new(200));
        let a_again = table.handle_for(EntityId::new(100));
        assert_eq!(a.raw(), 0);
        assert_eq!(b.raw(), 1);
        assert_eq!(a_again, a);
    }

    #[test]
    fn two_tables_assign_independent_handles_for_the_same_entity() {
        let mut table1 = HandleTable::new();
        let mut table2 = HandleTable::new();
        // Table 2 sees a decoy entity first, so the same entity lands on a different handle
        // number than in table 1 — demonstrating the tables do not share state.
        let _ = table2.handle_for(EntityId::new(999));

        let e = EntityId::new(42);
        let h1 = table1.handle_for(e);
        let h2 = table2.handle_for(e);
        assert_eq!(h1.raw(), 0);
        assert_eq!(h2.raw(), 1);
        assert_eq!(table1.entity_of(h1), Some(e));
        assert_eq!(table2.entity_of(h2), Some(e));
    }

    #[test]
    fn entity_of_unminted_handle_is_none() {
        let mut table = HandleTable::new();
        let _ = table.handle_for(EntityId::new(1));
        assert_eq!(table.entity_of(Handle::new(41)), None);
    }
}
