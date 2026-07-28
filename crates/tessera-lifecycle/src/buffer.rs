//! The ingest buffer: replayed `WalRow`s not yet in any segment (task-10 brief).
//!
//! Phase 1 has no flush — an ingested item is durable (WAL-fsynced) and participates in
//! authorisation state (I1 composition rule 4) the moment it's accepted, but has no row geometry
//! until the next `tessera build` folds it into a bundle. This is a stated Phase 1 limitation
//! (plan §5's WAL rationale is durability semantics, not visibility latency), not a bug: a
//! buffered item simply has no `Permutation::row_of` entry anywhere, so it can never contribute
//! to a tile's geometry, only (if its terms are satisfied) to the *count* — and even that only
//! once a future task teaches the composition path to synthesise a row for it, which Phase 1
//! does not do.
//!
//! Term descriptors are resolved through [`DescriptorResolver`]: the bundle dictionary first,
//! then a deterministic in-memory extension interned in replay order for descriptors the
//! dictionary has never seen. A term that lives only in the extension is unsatisfiable by any
//! session's `satisfied` set (which is computed from the auth plugin's grant against terms the
//! *auth* side knows about — an in-memory-only *data*-side term id is never handed to a viewer's
//! credential evaluation), so this is fail-closed, not fail-open: a novel descriptor can buffer
//! an item, but cannot make it visible, until the next build assigns it a durable term id.

use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_types::{EntityId, TermId};

use crate::wal::{WalRow, WalScalar};

/// Resolves term descriptors to `TermId`s: the bundle dictionary first, then a deterministic
/// (replay-order) in-memory extension for descriptors the dictionary has never interned.
///
/// Determinism obligation: called in WAL replay order (or live-accept order, which is the same
/// append order), so the same WAL byte-for-byte always yields the same extension assignment —
/// this is what makes replay reproducible across restarts, not merely "some valid resolution".
pub struct DescriptorResolver<'a> {
    dict: &'a Dict,
    extension: FxHashMap<Vec<u8>, TermId>,
    next_extension_id: u32,
}

impl<'a> DescriptorResolver<'a> {
    pub fn new(dict: &'a Dict) -> Self {
        DescriptorResolver {
            dict,
            extension: FxHashMap::default(),
            next_extension_id: dict.len(),
        }
    }

    /// Resolve one descriptor: a dictionary hit returns the durable, bundle-relative `TermId`
    /// unchanged; a miss is interned into the in-memory extension (assigning the next ordinal
    /// past the dictionary's own range) and that assignment is reused for any repeat of the same
    /// descriptor within this resolver's lifetime.
    pub fn resolve(&mut self, descriptor: &[u8]) -> TermId {
        if let Some(id) = self.dict.lookup(descriptor) {
            return id;
        }
        if let Some(&id) = self.extension.get(descriptor) {
            return id;
        }
        let id = TermId::new(self.next_extension_id);
        self.next_extension_id += 1;
        self.extension.insert(descriptor.to_vec(), id);
        id
    }
}

/// One buffered item's authorisation-relevant state: its resolved terms and the geometry/scalars
/// carried by its WAL row (kept for when a future flush gives it a row; unused by Phase 1
/// composition, which only reads `terms`).
#[derive(Debug, Clone, PartialEq)]
pub struct BufferedItem {
    pub terms: Vec<TermId>,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<WalScalar>,
}

/// Replayed `WalRow`s not yet folded into a bundle, keyed by (internal) `EntityId` — the id the
/// row was allocated under (SA §6.2: WAL rows carry their already-allocated id; replay reuses it,
/// never re-allocates).
#[derive(Debug, Default)]
pub struct IngestBuffer {
    items: FxHashMap<EntityId, BufferedItem>,
}

impl IngestBuffer {
    pub fn new() -> Self {
        IngestBuffer {
            items: FxHashMap::default(),
        }
    }

    /// Insert one WAL row's item, resolving its term descriptors via `resolver`.
    pub fn insert_row(&mut self, row: &WalRow, resolver: &mut DescriptorResolver<'_>) {
        let terms = row
            .descriptors
            .iter()
            .map(|d| resolver.resolve(d))
            .collect();
        self.insert_row_with_terms(row, terms);
    }

    /// Insert one WAL row's item with already-resolved `terms`, bypassing descriptor resolution.
    /// Exposed for tests (and any future caller that already holds resolved `TermId`s, e.g. a
    /// live-accept path that resolved descriptors once up front); production replay should
    /// normally go through [`Self::insert_row`].
    pub fn insert_row_with_terms(&mut self, row: &WalRow, terms: Vec<TermId>) {
        self.items.insert(
            row.entity_id,
            BufferedItem {
                terms,
                x: row.x,
                y: row.y,
                scalars: row.scalars.clone(),
            },
        );
    }

    pub fn get(&self, entity: EntityId) -> Option<&BufferedItem> {
        self.items.get(&entity)
    }

    pub fn contains(&self, entity: EntityId) -> bool {
        self.items.contains_key(&entity)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&EntityId, &BufferedItem)> {
        self.items.iter()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn dict_with(descriptors: &[&[u8]]) -> (Dict, TempDir) {
        let temp = TempDir::new().unwrap();
        let mut writer = tessera_authz::DictWriter::new(temp.path());
        for d in descriptors {
            writer.intern(d);
        }
        let paths = writer.finish().unwrap();
        (Dict::load(&paths).unwrap(), temp)
    }

    #[test]
    fn resolver_prefers_dict_then_extends_deterministically() {
        let (dict, _temp) = dict_with(&[b"1207", b"9"]);
        let mut resolver = DescriptorResolver::new(&dict);

        assert_eq!(resolver.resolve(b"1207"), TermId::new(0));
        assert_eq!(resolver.resolve(b"9"), TermId::new(1));
        // Novel descriptor: extension starts at dict.len() == 2.
        assert_eq!(resolver.resolve(b"novel"), TermId::new(2));
        // Repeat resolves to the same extension id.
        assert_eq!(resolver.resolve(b"novel"), TermId::new(2));
        // A second distinct novel descriptor gets the next ordinal.
        assert_eq!(resolver.resolve(b"novel-2"), TermId::new(3));
    }
}
