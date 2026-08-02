//! What a flush takes from the buffer, and the two states in which it takes nothing (§3.5).
//!
//! Planning is the executor's half of a flush: it runs against the live generation, decides which
//! buffered items acquire geometry, and hands an immutable plan to the background pool. Writing the
//! segment and publishing it are elsewhere — this module is the part where a disposition decides an
//! item's fate, which is the part that is invariant-bearing.
//!
//! ## The rules are relative to the buffer snapshot the flush took
//!
//! A delete accepted *after* the snapshot produces a deleted entity that **does** have a row,
//! hidden by its standing overlay entry alone. That is safe today only because nothing retires —
//! deletion denies never retire, there being no stamp ledger — and it is an obligation the
//! compaction spec inherits rather than a caveat this one absorbs.
//!
//! ## The three dispositions do different things, and uniformity here is fail-open
//!
//! Lifecycle §3.1 gives each a different relationship to the postings, so each gets a different
//! answer:
//!
//! - **Suppressed → flushed normally.** A suppression never touches postings and retires only on
//!   unsuppress, so a flush that skipped it would leave a later unsuppress with **nothing to
//!   reveal** — the item would have no row, and unsuppressing it would show nothing.
//! - **Deleted → never written into the segment.** The ID stays burned (I9), no row is created,
//!   and the deny entry stands.
//! - **Carrying an evaluate entry → the buffered row's terms are written, and the entry stands.**
//!   Writing the *entry's* current terms instead would be the fold, which is invariant-bearing and
//!   compaction's. This is the sentence that stops the fold arriving as a simplification.

use tessera_lifecycle::{BufferedItem, Overlay};
use tessera_types::EntityId;

use crate::Generation;

/// One flush's immutable plan: the items of one slice that will acquire geometry.
///
/// The slice is not carried: `plan_flush` is called per slice and the caller already holds it, so
/// a copy here would be a second answer to a question that has one.
#[derive(Debug)]
pub(crate) struct FlushPlan {
    /// **Ascending by entity id, deleted entities already removed.** Contiguity is I9's doing —
    /// ids are issued monotonically from the high-water — and it is what makes the segment's
    /// extent dense.
    ///
    /// The segment's entity range is this list's ends, and is deliberately not carried separately:
    /// a deleted entity at either end contributes no row, so a range taken from the *buffer's*
    /// bounds would claim one it does not have.
    pub(crate) items: Vec<(EntityId, BufferedItem)>,
}

/// Why a tick published nothing. Each is a distinct operator-facing condition, and two of them are
/// fail-closed postures rather than absences of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFlush {
    /// Nothing buffered for this slice, or everything buffered for it is deleted.
    NothingToFlush,
    /// **The WAL is poisoned** (§3.5). Under the apply-anyway rule an under-durable delete is in
    /// force in memory and was answered 500, and contracts §3.1's residual is that a restart makes
    /// the item visible again. A flush honouring such a delete would skip the entity and advance
    /// the watermark past it; replay would then discard the delete record, leaving the item in no
    /// segment and no buffer — the un-acked delete made **permanent**.
    ///
    /// Costs ingest visibility during WAL degradation, when nothing new is being made durable
    /// anyway.
    WalPoisoned,
    /// **The in-memory overlay has diverged from the durable WAL** (§7.2).
    ///
    /// `Wal::discard_undurable` deliberately does not un-apply — "a restart will not carry them" —
    /// so after an in-process recovery the node returns to `Running` while holding dispositions no
    /// record backs, and the poisoned gate no longer covers it. Publishing a manifest from that
    /// overlay would make a 500'd, never-acked deny **permanent**, contradicting contracts §3.1's
    /// residual.
    ///
    /// A diverged node keeps serving and keeps applying denies, but publishes no flush and rotates
    /// no WAL until it is restarted, alarmed throughout. Re-appending the divergent entries to
    /// converge the WAL was the alternative, and it is rejected because it produces a state **no
    /// restart could have produced** — which is lifecycle §4's central argument.
    OverlayDiverged,
}

/// Plan a flush of `slice` against `generation`.
///
/// Pure: it reads the generation and nothing else, so the same generation always yields the same
/// plan. The two postures are passed in rather than read here, because they are the executor's
/// health and not the generation's.
pub(crate) fn plan_flush(
    generation: &Generation,
    slice: &str,
    wal_poisoned: bool,
    overlay_diverged: bool,
) -> Result<FlushPlan, NoFlush> {
    // The gates first, and before any work: a poisoned or diverged node publishes nothing, and
    // deciding that after building a plan would only mean building one to throw away.
    if wal_poisoned {
        return Err(NoFlush::WalPoisoned);
    }
    if overlay_diverged {
        return Err(NoFlush::OverlayDiverged);
    }

    let mut items: Vec<(EntityId, BufferedItem)> = generation
        .buffer
        .iter()
        .filter(|(entity, item)| item.slice == slice && !is_deleted(&generation.overlay, **entity))
        .map(|(entity, item)| (*entity, item.clone()))
        .collect();
    if items.is_empty() {
        return Err(NoFlush::NothingToFlush);
    }
    // The buffer is a hash map, so order is arbitrary until sorted. Ascending by entity id is what
    // `write_flush_segment` requires and what makes the extent dense.
    items.sort_unstable_by_key(|(entity, _)| entity.raw());

    Ok(FlushPlan { items })
}

/// Whether `entity` is deleted as of this overlay.
///
/// **Only `deleted` excludes an item from a flush.** `suppressed` does not — the row must exist for
/// a later unsuppress to reveal — and `evaluate_terms` does not, because the terms written are the
/// buffered row's and the entry stands. Reading any other field here is the fold arriving as a
/// simplification; see this module's doc.
fn is_deleted(overlay: &Overlay, entity: EntityId) -> bool {
    overlay.get(entity).is_some_and(|entry| entry.deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use std::collections::{BTreeMap, HashMap};

    use tessera_lifecycle::wal::{ChangeOp, WalRow, WalScalar};
    use tessera_lifecycle::IngestBuffer;
    use tessera_store::manifest::{IdentityDescriptor, Manifest, Quantisation};
    use tessera_store::Bundle;
    use tessera_types::{TermId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

    const SLICE: &str = "s0";

    fn item(terms: &[u32]) -> BufferedItem {
        BufferedItem {
            terms: terms.iter().map(|t| TermId::new(*t)).collect(),
            slice: SLICE.to_string(),
            x: 0.5,
            y: 0.5,
            scalars: vec![WalScalar::U64(1)],
        }
    }

    fn buffer_with(buffered: &[(u64, BufferedItem)]) -> IngestBuffer {
        let mut buffer = IngestBuffer::new();
        for (entity, item) in buffered {
            let row = WalRow {
                external_id: Some(format!("ext-{entity}").into_bytes()),
                entity_id: EntityId::new(*entity),
                slice: item.slice.clone(),
                descriptors: Vec::new(),
                x: item.x,
                y: item.y,
                scalars: item.scalars.clone(),
            };
            buffer.insert_row_with_terms(&row, item.terms.clone());
        }
        buffer
    }

    /// A generation over `buffered`, with `changes` applied to its overlay.
    ///
    /// The bundle is empty: `plan_flush` reads the buffer and the overlay and nothing else, so a
    /// real one would make these tests about the fixture instead.
    fn generation_with(
        buffered: &[(u64, BufferedItem)],
        changes: &[(u64, ChangeOp)],
    ) -> Generation {
        let mut overlay = Overlay::new();
        for (entity, op) in changes {
            overlay.apply(EntityId::new(*entity), *op, None);
        }
        generation_of(overlay, buffer_with(buffered))
    }

    fn generation_of(overlay: Overlay, buffer: IngestBuffer) -> Generation {
        let manifest = Manifest {
            bundle_format: 1,
            created_at: "2026-08-02T00:00:00Z".to_string(),
            data_plugin_hash: "builtin:passthrough:1".to_string(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: vec![],
            small_term_threshold: 32,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            entity_id_high_water: 0,
            identity: IdentityDescriptor {
                construction: IDENTITY_CONSTRUCTION.to_string(),
                rounds: IDENTITY_ROUNDS,
                key: "0123456789abcdef0123456789abcdef".to_string(),
                shard_id: 0,
                idset: 1,
            },
            slices: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: BTreeMap::new(),
        };
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let postings_path = dir.path().join("postings.arrow");
        tessera_authz::write_postings(&postings_path, &[], 32).expect("an empty postings file");
        Generation {
            prefix: "v00000".to_string(),
            segments_version: 0,
            watermark: 0,
            bundle: Arc::new(Bundle {
                manifest,
                partitions: HashMap::new(),
            }),
            dict: Arc::new(tessera_authz::Dict::load(&[]).expect("an empty dict")),
            postings: Arc::new(
                tessera_authz::PostingsReader::open(&postings_path, false).expect("it opens"),
            ),
            delta_postings: Vec::new(),
            overlay_version: 0,
            overlay: Arc::new(overlay),
            buffer: Arc::new(buffer),
        }
    }

    fn plan(generation: &Generation) -> Result<FlushPlan, NoFlush> {
        plan_flush(generation, SLICE, false, false)
    }

    /// **A suppression never touches postings and retires only on unsuppress**, so a flush that
    /// skipped it would leave a later unsuppress with nothing to reveal: no row would exist, and
    /// unsuppressing the item would show nothing at all.
    #[test]
    fn a_suppressed_entity_is_flushed_so_a_later_unsuppress_has_something_to_reveal() {
        let generation = generation_with(&[(7, item(&[1]))], &[(7, ChangeOp::Suppress)]);
        let plan = plan(&generation).expect("a suppressed item still flushes");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].0, EntityId::new(7));
    }

    /// A deletion's ID stays burned (I9), no row is created, and the deny entry stands.
    #[test]
    fn a_deleted_entity_acquires_no_row() {
        let generation = generation_with(
            &[(7, item(&[1])), (8, item(&[1]))],
            &[(7, ChangeOp::Delete)],
        );
        let plan = plan(&generation).expect("the undeleted item still flushes");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(
            plan.items[0].0,
            EntityId::new(8),
            "the deleted entity contributes no row"
        );
    }

    /// **Writing the evaluate entry's current terms would be the fold**, which is
    /// invariant-bearing and compaction's. The buffered row's terms are what the tier carries, and
    /// the entry stands.
    #[test]
    fn an_evaluate_entry_leaves_the_buffered_rows_terms_alone() {
        let mut overlay = Overlay::new();
        overlay.apply(
            EntityId::new(7),
            ChangeOp::Predicate,
            Some(vec![TermId::new(99)]),
        );
        let generation = generation_of(overlay, buffer_with(&[(7, item(&[1]))]));

        let plan = plan(&generation).expect("an evaluate entry does not stop a flush");
        assert_eq!(
            plan.items[0].1.terms,
            vec![TermId::new(1)],
            "the WAL row's terms, never the entry's — writing the entry's is the fold"
        );
    }

    /// A deletion accepted *after* the snapshot is a different case, and is not this one's: it
    /// produces a deleted entity that **does** have a row, hidden by its overlay entry alone. Safe
    /// only because nothing retires, and an obligation the compaction spec inherits.
    #[test]
    fn a_delete_arriving_after_the_plan_does_not_unwrite_the_row() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        let plan = plan(&generation).expect("nothing is denied at the snapshot");
        assert_eq!(plan.items.len(), 1, "the row is planned");
        // A later delete cannot reach this plan: it is a value, taken from one generation.
        let later = generation_with(&[(7, item(&[1]))], &[(7, ChangeOp::Delete)]);
        assert!(matches!(
            plan_flush(&later, SLICE, false, false),
            Err(NoFlush::NothingToFlush)
        ));
    }

    /// **A `WalPoisoned` node publishes nothing** (§3.5). A flush honouring an under-durable
    /// delete would skip the entity and advance the watermark past it; replay would then discard
    /// the delete record, leaving the item in no segment and no buffer — the un-acked delete made
    /// permanent, against contracts §3.1's residual that a restart makes it visible again.
    #[test]
    fn a_wal_poisoned_node_plans_nothing() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        assert!(matches!(
            plan_flush(&generation, SLICE, true, false),
            Err(NoFlush::WalPoisoned)
        ));
    }

    /// **A node whose overlay has diverged from its durable WAL publishes nothing** (§7.2), and
    /// the poisoned gate does not cover it: `discard_undurable` returns the node to `Running`
    /// while it still holds dispositions no record backs.
    #[test]
    fn a_diverged_node_plans_nothing_even_though_its_wal_is_healthy() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        assert!(matches!(
            plan_flush(&generation, SLICE, false, true),
            Err(NoFlush::OverlayDiverged)
        ));
    }

    /// Items of another slice are not this slice's to flush: a segment's entity range is
    /// contiguous only within one (§2.1).
    #[test]
    fn another_slices_items_are_left_alone() {
        let mut other = item(&[1]);
        other.slice = "elsewhere".to_string();
        let generation = generation_with(&[(7, other)], &[]);
        assert!(matches!(plan(&generation), Err(NoFlush::NothingToFlush)));
    }

    /// Ascending by entity id, because `write_flush_segment` requires it and because the extent is
    /// dense over the range. The buffer is a hash map, so nothing else establishes the order.
    #[test]
    fn the_plan_is_ascending_by_entity_id() {
        let generation = generation_with(&[(9, item(&[1])), (3, item(&[1])), (7, item(&[1]))], &[]);
        let plan = plan(&generation).unwrap();
        let ids: Vec<u64> = plan.items.iter().map(|(e, _)| e.raw()).collect();
        assert_eq!(ids, vec![3, 7, 9]);
    }
}
