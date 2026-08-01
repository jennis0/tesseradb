//! Per-session handle tables.
//!
//! **Retired from the viewer plane by owner decision (2026-07-29; contracts §0.3
//! deviation 8).** Handles existed because entity IDs could not cross the trust
//! boundary and the gather had nothing else to show. `columns.arrow` now carries a
//! `tessera_id` at the row — a keyed permutation of `(shard, entity)` that is
//! order-free and invertible only inside the boundary — so the engine shows an
//! identity that was always safe to show, and a point's identity is stable across
//! sessions, which is what lets a client bookmark, share and reconcile it. I10 is not
//! weakened by the retirement: it is what made the retirement possible, since after
//! contracts r6 no request-path artifact stores an entity ID at all (see
//! `tessera_engine::viewport::row_to_point`).
//!
//! Kept, not deleted, because Phase 3's node handles (`/v1/labels` returns
//! `node_handle`) are genuinely per-session and need exactly this machinery — a
//! frontier node is a query-time object rather than a corpus object, so there is no
//! stable identity to permute — and because deleting the type would delete the
//! constraint below with it. **Nothing allocates a table today**: no per-session
//! entry holds one, because there is nothing to put in it until that verb exists.
//!
//! **The constraint a node handle must obey, which is what this type records.** A
//! decoded worker-local reference is an **index into the worker's own table, never an
//! entity ID**. The temptation under router/worker fan-out is to put the entity ID
//! into the permutation's plaintext and let the router invert it; that would ship
//! corpus identifiers to the router, which is precisely what I10 forbids and what the
//! per-partition isolation of the compartmented design assumes does not happen. The
//! table is the indirection that makes the plaintext local, and the encoding hardening
//! described below sits on top of it rather than replacing it.
//!
//! **Sequential-mint leak rationale** (preserved from the viewer-plane design this module
//! originally served, and still the rationale a future Phase 3 caller inherits). `handle_for`
//! mints handles `0, 1, 2, …` in first-visit order within a session. That order is not free of
//! information: it discloses the sequence in which distinct entities were first returned to
//! *this* viewer across *their own* requests. But the viewer already observes that order
//! directly — it is exactly the order in which their own viewport/pan/zoom calls surfaced those
//! entities to them in the first place — so encoding it into the handle assignment discloses
//! nothing beyond what the viewer's own request history already tells them. It does **not**
//! disclose anything about *other* sessions (each table is independent, see the isolation test
//! below), nor about entity IDs, entity-ID density, or insertion order in the underlying store.
//! The one property this construction deliberately defers is SA §4.5's per-session **keyed
//! permutation** encoding, which additionally hides within-session visit order from a viewer
//! correlating handles across their own requests over time (e.g. to infer whether two viewport
//! calls re-surfaced the same entity without it being obviously "the same handle again"); that
//! hardening arrives with router/worker fan-out, not Phase 1's single-process walking skeleton.

use std::collections::HashMap;

use tessera_types::{EntityId, Handle};

/// A per-session mapping between `EntityId` and per-session opaque `Handle`.
///
/// Handles are minted sequentially on first sight of an entity within *this* table and are
/// stable for the table's lifetime; a fresh `HandleTable` (e.g. a new session) starts its
/// numbering over, so the same entity gets an independent handle in each session (I10).
// Phase 3: node handles (`/v1/labels`'s `node_handle`) are genuinely per-session and will
// consume this type; until then nothing on the viewer plane constructs one.
#[allow(dead_code)]
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
