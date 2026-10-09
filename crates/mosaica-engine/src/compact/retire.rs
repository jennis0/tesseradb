use croaring::Bitmap;

use mosaica_store::permutation::SegmentExtent;

/// The entities every carried-forward segment can name: each dense span whole,
/// and each entity listed below one. Naming too many keeps a tombstone for another fold; naming
/// too few exposes a deleted item. Built at publication, because built at plan time it would miss
/// flushes published while the fold ran.
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

    /// A flush's tier lies within the entities its segment holds, so the segment's span and listed
    /// rows cover it, including an item with no terms.
    pub(crate) fn add_segment(&mut self, extent: &SegmentExtent) {
        self.add_range(extent.entity_lo, extent.entity_hi);
        self.entities
            .add_many(&extent.below.iter().map(|&(entity, _)| entity).collect::<Vec<_>>());
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

    fn segment(seg_id: &str, entity_lo: u64, entity_hi: u64) -> SegmentExtent {
        SegmentExtent::from_rows(seg_id, 0, (entity_lo, entity_hi), entity_lo..=entity_hi)
            .expect("a dense extent")
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
