use croaring::Bitmap;

use tessera_store::manifest::SegmentDescriptor;

/// Every entity a carried-forward artefact names; [`executed`] subtracts it from the tombstone
/// set. Built at publication, from the live partition manifest minus what the fold consumed.
/// Built at plan time it would miss flushes that publish while the fold runs.
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

    /// A segment the fold did not consume. Takes its whole declared entity range, not only the
    /// entities it holds: narrowing the range by reading the segment would risk naming fewer
    /// entities than it actually carries, which exposes a deleted item.
    pub(crate) fn add_segment(&mut self, descriptor: &SegmentDescriptor) {
        self.add_range(descriptor.entity_lo, descriptor.entity_hi);
    }

    /// A locator extent the fold did not consume. Covers the external-id binding of an item with
    /// no terms, which no tier names.
    pub(crate) fn add_locator_extent(&mut self, extent: &tessera_store::manifest::LocatorExtent) {
        self.add_range(extent.entity_lo, extent.entity_hi);
    }

    /// `entity_lo ..= entity_hi`, inclusive. Entity ids are capped at `u32::MAX`. A range that
    /// does not fit is clamped outward to that maximum rather than dropped, so it still names
    /// every entity it covers.
    fn add_range(&mut self, entity_lo: u64, entity_hi: u64) {
        if entity_lo > entity_hi {
            return;
        }
        let lo = u32::try_from(entity_lo).unwrap_or(u32::MAX);
        let hi = u32::try_from(entity_hi).unwrap_or(u32::MAX);
        self.entities.add_range(lo..=hi);
    }

    /// How many entities are carried forward, for the publication's log line.
    pub(crate) fn len(&self) -> u64 {
        self.entities.cardinality()
    }
}

/// The deletions whose overlay entries retire in this fold: the tombstone set minus everything a
/// carried-forward artefact names. An entity left in the result has lost both its row and its
/// postings to this fold's passes. Passes 1–3 run over the tombstone set itself, never over this.
pub(crate) fn executed(d0: &Bitmap, carried: &CarriedForward) -> Bitmap {
    let mut executed = d0.clone();
    executed.andnot_inplace(&carried.entities);
    executed
}

#[cfg(test)]
mod tests {
    use super::*;

    use tessera_store::manifest::LocatorExtent;

    fn segment(seg_id: &str, entity_lo: u64, entity_hi: u64) -> SegmentDescriptor {
        SegmentDescriptor {
            incarnation: 0,
            view: "s0".to_string(),
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
