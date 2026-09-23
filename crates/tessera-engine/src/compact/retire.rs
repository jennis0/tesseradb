use croaring::Bitmap;

use tessera_store::manifest::SegmentDescriptor;

/// The whole entity range of every carried-forward segment and locator extent. Naming too many
/// keeps a tombstone for another fold; naming too few exposes a deleted item. Built at
/// publication, because built at plan time it would miss flushes published while the fold ran.
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

    /// A flush publishes segment, tier, run and locator extent over one contiguous entity range,
    /// so the segment's range covers them all, including an item with no terms.
    pub(crate) fn add_segment(&mut self, descriptor: &SegmentDescriptor) {
        self.add_range(descriptor.entity_lo, descriptor.entity_hi);
    }

    pub(crate) fn add_locator_extent(&mut self, extent: &tessera_store::manifest::LocatorExtent) {
        self.add_range(extent.entity_lo, extent.entity_hi);
    }

    /// Inclusive. A range past `u32::MAX` is clamped outward to it, never dropped.
    fn add_range(&mut self, entity_lo: u64, entity_hi: u64) {
        if entity_lo > entity_hi {
            return;
        }
        let lo = u32::try_from(entity_lo).unwrap_or(u32::MAX);
        let hi = u32::try_from(entity_hi).unwrap_or(u32::MAX);
        self.entities.add_range(lo..=hi);
    }

    pub(crate) fn len(&self) -> u64 {
        self.entities.cardinality()
    }
}

/// The deletions whose overlay entries retire in this fold. The passes run over the tombstone set
/// itself, not over this.
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

    /// A deletion nothing carried forward retires; one a carried segment names does not.
    #[test]
    fn a_deletion_the_fold_removed_retires_and_one_still_carried_does_not() {
        let d0 = bitmap(&[7, 9, 50]);
        let mut carried = CarriedForward::new();
        // A flush published while the fold ran.
        carried.add_segment(&segment("flush-3-1", 40, 60));

        let executed = executed(&d0, &carried);

        assert!(executed.contains(7));
        assert!(executed.contains(9));
        assert!(!executed.contains(50), "a carried entity retired");
    }

    /// A carried range over entities that were never deleted changes nothing.
    #[test]
    fn nothing_outside_d0_can_retire() {
        let d0 = bitmap(&[3]);
        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("flush-3-1", 100, 200));

        let executed = executed(&d0, &carried);

        assert_eq!(executed.cardinality(), 1);
        assert!(executed.contains(3));
        assert!(
            executed.andnot(&d0).is_empty(),
            "an entity never deleted retired"
        );
    }

    /// The segment and the locator extent cover disjoint ranges, so each protects its own
    /// entities with no help from the other.
    #[test]
    fn a_carried_segment_and_a_carried_locator_extent_each_protect_their_range() {
        // 51 and 53: postings and a row, named only by the segment.
        // 55: an item with no terms, named only by the locator extent.
        let d0 = bitmap(&[51, 53, 55, 90]);

        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("flush-3-1", 50, 53));
        carried.add_locator_extent(&locator(54, 56));

        let executed = executed(&d0, &carried);

        for entity in [51u32, 53, 55] {
            assert!(!executed.contains(entity), "carried {entity} retired");
        }
        assert!(executed.contains(90), "an uncarried deletion was kept");
    }

    /// A carried locator extent protects its range even with no segment beside it.
    #[test]
    fn a_locator_extent_alone_protects_its_range() {
        let d0 = bitmap(&[12]);
        let mut carried = CarriedForward::new();
        carried.add_locator_extent(&locator(10, 20));

        assert!(executed(&d0, &carried).is_empty());
    }

    #[test]
    fn with_nothing_carried_forward_every_tombstone_retires() {
        let d0 = bitmap(&[1, 2, 3]);
        let executed = executed(&d0, &CarriedForward::new());
        assert_eq!(executed.cardinality(), 3);
    }

    /// A range past `u32::MAX` is clamped outward, so both entities stay protected.
    #[test]
    fn an_out_of_range_carry_forward_clamps_outward_never_dropping_protection() {
        let d0 = bitmap(&[u32::MAX, u32::MAX - 1]);
        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("huge", u32::MAX as u64 - 1, u32::MAX as u64 + 100));

        assert!(
            executed(&d0, &carried).is_empty(),
            "the clamped range lost an entity"
        );
    }
}
