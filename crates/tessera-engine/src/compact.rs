//! The compaction fold — plan, execute, publish (compaction §1).
//!
//! ⊘ **No fold runs yet.** What is built here is the publication-time half that decides *which
//! deletions retire*, which is the one part of the fold where a mistake is fail-open rather than
//! merely wrong. The passes it will drive are built and live in `tessera-store` and `tessera-authz`
//! (`fold_row_space`, `sweep_term_postings`, `fold_external_id_runs`); the plan, the dedicated
//! thread that runs them, and the publication that carries their output through the seam are not.
//!
//! # Retirement, and why the obvious definition is fail-open
//!
//! Rule F (write-path §5.4): a deletion's overlay entry leaves `deleted` only at the fold that
//! **executes** it. The tempting definition of "executes" is the plan's own tombstone clone `D₀` —
//! the set the passes ran over — and it serves an acknowledged deletion permanently, by an
//! interleaving nothing in the fold can see:
//!
//! > A flush plans at tick *N* with entity E buffered. Its pool run spans the tick, since nothing
//! > bounds flush and fold overlap. A delete for E is accepted, so `D₀ ∋ E` at tick *N+1*. But the
//! > flush's segment is not in the fold's file list, so the fold removes neither E's row nor its
//! > postings; the flush then publishes both into the old prefix, the fold carries that segment and
//! > its tier forward verbatim, and retirement withdraws the only thing hiding E. E is drawn,
//! > counted and served to every authorised principal.
//!
//! **The identity match cannot catch this** — no fragment is stale, and E genuinely is in the
//! post-fold postings. So retirement is derived from what the publication demonstrably removed,
//! never from what the plan predicted it would (compaction §5):
//!
//! > `executed = { e ∈ D₀ : no carried-forward artefact names e }`, evaluated at publication —
//! > **artefact meaning tier, segment *and* external-id run, not tier alone.**
//!
//! # The safety property is that the carry-forward set is an over-approximation
//!
//! [`executed`] subtracts, so every entity the carry-forward set names is one that does **not**
//! retire. Naming too many is fail-closed — an un-retired tombstone keeps hiding an item that is
//! already gone, costs one overlay entry, and the next fold takes it. Naming too few is the
//! fail-open above. Every judgement in [`CarriedForward`] is therefore made in the direction of
//! naming more, and where a cheap over-approximation is available it is preferred to an exact
//! answer that could be wrong.
//!
//! That is also the answer to the question compaction §5 raises and `DeltaTier` cannot answer —
//! *"how do you ask a tier whether it contains an entity"*. You do not. A flush publishes a
//! segment, a tier, a run and a locator extent **together, over one contiguous entity range**, and
//! merge and coalesce are suspended for the fold's duration (compaction §1), so the segment's own
//! `entity_lo..=entity_hi` covers every entity the other three can name. Taking the range covers
//! the tier without a primitive that does not exist, and it covers the case a tier-based test
//! misses outright: **a zero-term item produces no `(term, entity)` pair at all** — nothing on the
//! ingest path refuses one, and the reference plugin drops empty descriptors — so no tier names it
//! while its row and its external-id binding are both carried forward. Retiring it would 409 a
//! lawful re-ingest of its external id (decision 0047), which is why compaction §12's obligation 2b
//! names it as the shape a tier test misses.

// ⊘ **Nothing calls this yet, and the marker is deliberate.** The publication that will —
// compaction §4's step 6, which hands `executed` to `Engine::publish_rotated_prefix` — is the next
// part of this epic. The retirement rule is built ahead of it because it is the half of the fold
// where a mistake is fail-open, and it is testable in isolation while a whole fold is not: every
// case below is one of compaction §12's obligations, and none of them needs a fold to run. The
// alternative was to land the rule inside the publication and have its first test be an end-to-end
// one, which is how a rule this shape gets checked once and never again.
#![allow(dead_code)]

use croaring::Bitmap;

use tessera_store::manifest::{LocatorExtent, SegmentDescriptor};

/// Every entity a carried-forward artefact names — the operand [`executed`] subtracts from `D₀`.
///
/// Built at **publication**, from the live partition manifest minus what the fold consumed, and
/// never at plan time: computing it early means predicting which flushes will land during the
/// fold's flight, which is exactly the prediction compaction §5's rule replaced.
#[derive(Debug, Default)]
pub(crate) struct CarriedForward {
    entities: Bitmap,
}

impl CarriedForward {
    pub(crate) fn new() -> Self {
        CarriedForward {
            entities: Bitmap::new(),
        }
    }

    /// A segment the fold did not consume: every entity in its range is carried forward, and so is
    /// every entity its flush's tier and run name (see the module doc).
    ///
    /// **Its whole declared range, not the entities it demonstrably holds.** The range is what the
    /// manifest publishes and what a reader addresses it by; enumerating the `tessera_id` column to
    /// narrow it would cost a mapped read per carried segment to arrive at a *smaller* set, which
    /// is the fail-open direction.
    pub(crate) fn add_segment(&mut self, descriptor: &SegmentDescriptor) {
        self.add_range(descriptor.entity_lo, descriptor.entity_hi);
    }

    /// A locator extent the fold did not consume — the external-id half of compaction §5's
    /// "tier, segment *and* run", and the one that names a zero-term item's binding.
    pub(crate) fn add_locator_extent(&mut self, extent: &LocatorExtent) {
        self.add_range(extent.entity_lo, extent.entity_hi);
    }

    /// An entity named directly — the escape hatch for an artefact that carries no range, so that
    /// a future carried-forward kind is expressible without widening the fail-open.
    pub(crate) fn add_entity(&mut self, entity: u64) {
        self.add_range(entity, entity);
    }

    /// `entity_lo ..= entity_hi`, inclusive.
    ///
    /// Entity ids are capped at `u32::MAX` by the I9 allocator (contracts §2.6 r6), which is what
    /// lets the deny sets be Roaring bitmaps at all. A range that does not fit is **not** silently
    /// truncated: a truncated range names fewer entities, which is the fail-open direction, so the
    /// out-of-range part is clamped *outward* to the representable maximum rather than dropped.
    fn add_range(&mut self, entity_lo: u64, entity_hi: u64) {
        if entity_lo > entity_hi {
            return;
        }
        let lo = u32::try_from(entity_lo).unwrap_or(u32::MAX);
        let hi = u32::try_from(entity_hi).unwrap_or(u32::MAX);
        self.entities.add_range(lo..=hi);
    }

    /// How many entities are carried forward — a diagnostic for the publication's log line, so an
    /// operator can see a fold that retired nothing because everything was carried.
    pub(crate) fn len(&self) -> u64 {
        self.entities.cardinality()
    }
}

/// The deletions whose overlay entries retire in this fold's own swap: `D₀` minus everything a
/// carried-forward artefact names.
///
/// **`executed ⊆ D₀` is the safety property**, and it holds by construction here because this
/// function only ever subtracts. Everything that retires demonstrably lost its row and its
/// postings to the fold's own passes; an entity in `D₀ \ executed` has lost both as well and merely
/// keeps its overlay entry for another round, which is fail-closed and costs one bitmap entry.
///
/// **Passes 1–3 execute over `D₀`, never over this.** The containment is one-directional on
/// purpose: the passes ran hours before this is computable, and making them use it would require
/// computing it at plan time — predicting the carry-forward set, the fail-open compaction §5's rule
/// replaced. That is enforced by construction rather than by comment: the passes take their
/// tombstone set as a parameter in `tessera-store` and `tessera-authz`, crates from which this
/// function is not reachable.
pub(crate) fn executed(d0: &Bitmap, carried: &CarriedForward) -> Bitmap {
    let mut executed = d0.clone();
    executed.andnot_inplace(&carried.entities);
    executed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(seg_id: &str, entity_lo: u64, entity_hi: u64) -> SegmentDescriptor {
        SegmentDescriptor {
            slice: "s0".to_string(),
            seg_id: seg_id.to_string(),
            row_count: (entity_hi - entity_lo + 1) as u32,
            entity_lo,
            entity_hi,
        }
    }

    fn locator(entity_lo: u64, entity_hi: u64) -> LocatorExtent {
        LocatorExtent {
            path: "entities/ext-locator-1.u32".to_string(),
            entity_lo,
            entity_hi,
            external_id_run: "entities/external-ids-1.arrow".to_string(),
        }
    }

    fn bitmap(entities: &[u32]) -> Bitmap {
        Bitmap::of(entities)
    }

    /// **A deletion whose entity nothing carried forward retires; one that a carried-forward
    /// segment still names does not.** The base case compaction §5's rule exists for.
    ///
    /// **Mutations this kills:** retiring `D₀` wholesale (entity 50 retires, which is the r3
    /// fail-open); subtracting in the other direction (nothing retires).
    #[test]
    fn a_deletion_the_fold_removed_retires_and_one_still_carried_does_not() {
        let d0 = bitmap(&[7, 9, 50]);
        let mut carried = CarriedForward::new();
        // A flush that landed during the fold's flight, publishing entities 40..=60.
        carried.add_segment(&segment("flush-3-1", 40, 60));

        let executed = executed(&d0, &carried);

        assert!(executed.contains(7), "7 lost its row and its postings");
        assert!(executed.contains(9));
        assert!(
            !executed.contains(50),
            "50's row was published into the old prefix after the snapshot and carried forward — \
             retiring it withdraws the only thing hiding it"
        );
    }

    /// **`executed ⊆ D₀` always** — a carried-forward artefact naming entities that were never
    /// deleted changes nothing, and no entity outside `D₀` can ever retire.
    #[test]
    fn nothing_outside_d0_can_retire() {
        let d0 = bitmap(&[3]);
        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("flush-3-1", 100, 200));

        let executed = executed(&d0, &carried);

        assert_eq!(executed.cardinality(), 1);
        assert!(executed.contains(3));
        assert!(d0.andnot(&executed).is_empty() || executed.andnot(&d0).is_empty());
        assert!(
            executed.andnot(&d0).is_empty(),
            "executed must be a subset of D₀ — it is only ever a subtraction from it"
        );
    }

    /// **Obligation 2b's three shapes, each protected by a *different* carried artefact.**
    ///
    /// A carried-forward tier holds a deleted entity's postings; a carried-forward segment holds
    /// its row; and a carried-forward **run** holds its external-id binding while *no tier names it
    /// at all* — a zero-term item, which produces no `(term, entity)` pair because nothing on the
    /// ingest path refuses one. All three must survive the fold with their overlay entries intact.
    ///
    /// **The ranges are deliberately disjoint, and an earlier version of this test was not.** With
    /// one flush's segment and locator covering the same entities, either one alone protected all
    /// three shapes, so dropping `add_segment` entirely left this test green — it asserted the
    /// outcome without pinning what produced it. Two flushes, one contributing only a segment and
    /// the other only a locator extent, make each artefact kind independently load-bearing.
    ///
    /// **Mutations this kills:** dropping segments from the carry-forward set (51 and 53 retire —
    /// this is the "tiers alone" shape, since a tier's entities are covered by its flush's segment
    /// range and nothing else here names them); dropping locator extents (55 retires, which is the
    /// zero-term item whose binding a re-ingest would then hit as a 409, decision 0047).
    #[test]
    fn none_of_obligation_2bs_three_shapes_retires() {
        // 51: postings in a carried-forward tier. 53: a row in a carried-forward segment. Both are
        // covered by their flush's segment range and by nothing else in this fixture.
        // 55: a zero-term item, reachable here only through the run its locator extent names.
        let d0 = bitmap(&[51, 53, 55, 90]);

        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("flush-3-1", 50, 53));
        carried.add_locator_extent(&locator(54, 56));

        let executed = executed(&d0, &carried);

        for entity in [51u32, 53, 55] {
            assert!(
                !executed.contains(entity),
                "entity {entity} is named by a carried-forward artefact and must not retire"
            );
        }
        assert!(
            executed.contains(90),
            "a deletion nothing carried forward still retires — the rule must not be vacuous"
        );
    }

    /// **A run carried forward on its own still protects its entities**, so the rule does not
    /// depend on a segment always accompanying it. Merge and coalesce are suspended during a fold,
    /// which is what makes the segment range sufficient *today*; this keeps the rule correct if
    /// that ever gives, rather than resting on the suspension.
    #[test]
    fn a_locator_extent_alone_protects_its_range() {
        let d0 = bitmap(&[12]);
        let mut carried = CarriedForward::new();
        carried.add_locator_extent(&locator(10, 20));

        assert!(executed(&d0, &carried).is_empty());
    }

    /// **An empty carry-forward set retires the whole of `D₀`** — the quiet-deployment case, where
    /// no flush landed during the fold's flight. Stated because it is the one case where the rule
    /// and the fail-open definition agree, and a test suite that only covered it would prove
    /// nothing.
    #[test]
    fn with_nothing_carried_forward_every_tombstone_retires() {
        let d0 = bitmap(&[1, 2, 3]);
        let executed = executed(&d0, &CarriedForward::new());
        assert_eq!(executed.cardinality(), 3);
    }

    /// An entity range past `u32::MAX` clamps **outward**, naming more rather than fewer. A
    /// truncating cast would silently stop protecting the entities above the cut.
    #[test]
    fn an_out_of_range_carry_forward_clamps_outward_never_dropping_protection() {
        let d0 = bitmap(&[u32::MAX, u32::MAX - 1]);
        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("huge", u32::MAX as u64 - 1, u32::MAX as u64 + 100));

        assert!(
            executed(&d0, &carried).is_empty(),
            "both entities are inside the carried range once it is clamped outward"
        );
    }
}
