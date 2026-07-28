//! The overlay: per-entity deny/evaluate state accumulated from `/control/changes` (lifecycle
//! §3.1's three retirement rules, task-10 brief).
//!
//! `OverlayEntry` carries **three independent facts**, never one overwritable disposition:
//! `deleted`, `suppressed` and `evaluate_terms` retire on entirely different triggers (deletion
//! denies retire only via the epoch ledger, which does not exist until compaction lands;
//! suppressions retire only on `Unsuppress`; predicate changes retire at their compaction fold —
//! lifecycle §3). Collapsing them into a single enum ("last write wins") was caught fail-open in
//! review twice (CLAUDE.md): the sequence `delete → suppress → unsuppress` must not re-expose a
//! deleted item, and only three independent booleans/options — each cleared by nothing but its
//! own opposite operation, or (for `deleted`) by nothing at all in Phase 1 — make that
//! structurally impossible rather than merely tested-against.

use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_types::{EntityId, TermId};

use crate::buffer::{DescriptorResolver, IngestBuffer};
use crate::wal::{ChangeOp, WalRecord};

/// One entity's accumulated disposition. Default (all `false`/`None`) is "untouched" — never
/// constructed as a stand-in for "not deleted", only ever the actual absence of any change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayEntry {
    /// Set by `Delete`. Terminal in Phase 1: nothing clears it (no epoch ledger yet).
    pub deleted: bool,
    /// Set by `Suppress`; cleared **only** by `Unsuppress`. Never touched by `Delete` or
    /// `Predicate`.
    pub suppressed: bool,
    /// Set by `Predicate`. Replaces the fragment's verdict for this entity in both directions
    /// once present; never cleared by `Delete`/`Suppress`/`Unsuppress`.
    pub evaluate_terms: Option<Vec<TermId>>,
}

/// Accumulated overlay state, keyed by (internal) `EntityId`. Never keyed by external id —
/// external-id resolution happens once, at replay/accept time (see [`replay`]), so the hot
/// composition path (`tessera_engine::compose`) never has to resolve identity.
#[derive(Debug, Default)]
pub struct Overlay {
    entries: FxHashMap<EntityId, OverlayEntry>,
}

impl Overlay {
    pub fn new() -> Self {
        Overlay {
            entries: FxHashMap::default(),
        }
    }

    /// The entry for `entity`, if any change has ever touched it.
    pub fn get(&self, entity: EntityId) -> Option<&OverlayEntry> {
        self.entries.get(&entity)
    }

    /// Iterate every entity this overlay has an opinion on (a superset of entities with an
    /// *active* deny — an entry can be present but currently neutral, e.g. after
    /// `suppress → unsuppress`).
    pub fn iter(&self) -> impl Iterator<Item = (&EntityId, &OverlayEntry)> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Apply one disposition change to `entity`. The three facts are updated independently —
    /// see this module's doc. `descriptors` (already resolved to `TermId`s) is used only for
    /// `ChangeOp::Predicate`; ignored (should be `None`) for the other three ops.
    pub fn apply(&mut self, entity: EntityId, op: ChangeOp, terms: Option<Vec<TermId>>) {
        let entry = self.entries.entry(entity).or_default();
        match op {
            ChangeOp::Delete => entry.deleted = true,
            ChangeOp::Suppress => entry.suppressed = true,
            ChangeOp::Unsuppress => entry.suppressed = false,
            ChangeOp::Predicate => entry.evaluate_terms = terms,
        }
    }
}

/// Replay failures. Fail-closed (Global Constraint 3): a change naming an external id this
/// replay has never seen — neither via an `IngestBatch` row replayed so far, nor via
/// `resolve_from_bundle` (the bundle's `entities/external-ids-0.arrow` extent, wired in by the
/// caller) — must not be silently dropped or silently applied to the wrong entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// `404 unknown` (Reference Sheet R5): a `Change` record named an external id no known
    /// entity (bundle or WAL-established) has ever claimed.
    UnknownExternalId(Vec<u8>),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayError::UnknownExternalId(id) => {
                write!(f, "unknown external id: {id:02x?}")
            }
        }
    }
}

impl std::error::Error for OverlayError {}

/// Replay a WAL's records into an `Overlay` and an `IngestBuffer`.
///
/// Single left-to-right pass over `records`, in on-disk order (the order changes and ingests
/// were accepted in — WAL append order is causal order):
/// - `IngestBatch` rows populate `IngestBuffer` (term descriptors resolved via `dict` plus a
///   deterministic, replay-order in-memory extension — see [`DescriptorResolver`]) and register
///   `external_id → entity_id` in this replay's own external-id map, so a later `Change` record
///   in the *same* WAL naming that external id resolves without touching the bundle.
/// - `Change` records resolve their `external_id` first against this replay's own map, then
///   against `resolve_from_bundle` (entities established before this WAL — i.e. present in the
///   bundle's `entities/external-ids-0.arrow` extent, contracts §2.1); an id found in neither is
///   `OverlayError::UnknownExternalId` (`404 unknown`, R5) — fail-closed, never silently ignored.
///   `Predicate`'s descriptors are resolved through the same `DescriptorResolver` as ingest rows.
/// - `Lease` records carry no overlay/buffer information (I9 allocator bookkeeping only) and are
///   skipped here.
pub fn replay(
    records: &[WalRecord],
    dict: &Dict,
    resolve_from_bundle: impl Fn(&[u8]) -> Option<EntityId>,
) -> Result<(Overlay, IngestBuffer), OverlayError> {
    let mut overlay = Overlay::new();
    let mut buffer = IngestBuffer::new();
    let mut resolver = DescriptorResolver::new(dict);
    let mut established: FxHashMap<Vec<u8>, EntityId> = FxHashMap::default();

    for record in records {
        match record {
            WalRecord::IngestBatch { rows, .. } => {
                for row in rows {
                    established.insert(row.external_id.clone(), row.entity_id);
                    buffer.insert_row(row, &mut resolver);
                }
            }
            WalRecord::Change {
                external_id,
                op,
                descriptors,
            } => {
                let entity = established
                    .get(external_id.as_slice())
                    .copied()
                    .or_else(|| resolve_from_bundle(external_id))
                    .ok_or_else(|| OverlayError::UnknownExternalId(external_id.clone()))?;

                let terms = descriptors
                    .as_ref()
                    .map(|ds| ds.iter().map(|d| resolver.resolve(d)).collect());
                overlay.apply(entity, *op, terms);
            }
            WalRecord::Lease { .. } => {}
        }
    }

    Ok((overlay, buffer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::ChangeOp;

    #[test]
    fn three_facts_are_independent() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(1);

        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Suppress, None);
        overlay.apply(e, ChangeOp::Unsuppress, None);

        let entry = overlay.get(e).unwrap();
        assert!(entry.deleted, "delete must be terminal in Phase 1");
        assert!(!entry.suppressed, "unsuppress clears suppressed only");
    }

    #[test]
    fn unsuppress_never_clears_deleted_reverse_order() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(2);

        overlay.apply(e, ChangeOp::Suppress, None);
        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Unsuppress, None);

        let entry = overlay.get(e).unwrap();
        assert!(entry.deleted);
        assert!(!entry.suppressed);
    }

    #[test]
    fn predicate_never_clears_deny_flags() {
        let mut overlay = Overlay::new();
        let e = EntityId::new(3);

        overlay.apply(e, ChangeOp::Delete, None);
        overlay.apply(e, ChangeOp::Predicate, Some(vec![TermId::new(9)]));

        let entry = overlay.get(e).unwrap();
        assert!(entry.deleted);
        assert_eq!(entry.evaluate_terms, Some(vec![TermId::new(9)]));
    }

    #[test]
    fn replay_reports_unknown_external_id_as_404() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = tessera_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let records = vec![WalRecord::Change {
            external_id: b"never-ingested".to_vec(),
            op: ChangeOp::Suppress,
            descriptors: None,
        }];

        let result = replay(&records, &dict, |_external_id| None);
        assert_eq!(
            result.unwrap_err(),
            OverlayError::UnknownExternalId(b"never-ingested".to_vec())
        );
    }

    #[test]
    fn replay_resolves_change_against_bundle_when_not_established_by_this_wal() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let writer = tessera_authz::DictWriter::new(temp.path());
        let paths = writer.finish().unwrap();
        let dict = Dict::load(&paths).unwrap();

        let bundle_entity = EntityId::new(42);
        let records = vec![WalRecord::Change {
            external_id: b"from-a-previous-build".to_vec(),
            op: ChangeOp::Delete,
            descriptors: None,
        }];

        let (overlay, _buffer) = replay(&records, &dict, |external_id| {
            if external_id == b"from-a-previous-build" {
                Some(bundle_entity)
            } else {
                None
            }
        })
        .unwrap();

        assert!(overlay.get(bundle_entity).unwrap().deleted);
    }
}
