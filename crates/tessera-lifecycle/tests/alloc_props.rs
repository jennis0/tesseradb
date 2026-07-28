//! I9 allocator property tests (task-9 brief, Step 3): monotonicity, no reuse across simulated
//! crashes, and `assign_sorted`'s contiguous-signature grouping.

use std::collections::HashSet;

use proptest::prelude::*;

use tessera_lifecycle::alloc::{assign_sorted, Allocator, PendingItem};
use tessera_types::TermId;

proptest! {
    /// Interleaved `allocate` calls on a single allocator never overlap and always advance the
    /// high-water mark by exactly the amount requested.
    #[test]
    fn allocate_is_strictly_monotone(sizes in prop::collection::vec(1u64..64, 1..60)) {
        let mut alloc = Allocator::new(0);
        let mut expected_next = 0u64;
        let mut seen: HashSet<u64> = HashSet::new();

        for n in sizes {
            let range = alloc.allocate(n);
            prop_assert_eq!(range.start, expected_next);
            prop_assert_eq!(range.end, expected_next + n);
            for id in range.clone() {
                prop_assert!(seen.insert(id), "id {} allocated twice in one session", id);
            }
            expected_next = range.end;
            prop_assert_eq!(alloc.high_water(), expected_next);
        }
    }

    /// Simulates repeated crashes: each "session" allocates some ranges, all of which become
    /// durable (as WAL rows/leases would, once fsynced) before the allocator is dropped and
    /// rebuilt from `max(manifest_hw, replayed rows/leases)` — exactly `high_water()` at the
    /// point of the crash. No ID handed out in an earlier session may reappear in a later one.
    #[test]
    fn no_reuse_across_simulated_crashes(
        session_sizes in prop::collection::vec(prop::collection::vec(1u64..32, 1..10), 1..12),
    ) {
        let mut durable_hw = 0u64;
        let mut used: HashSet<u64> = HashSet::new();

        for sizes in session_sizes {
            // Rebuild from the durable high-water mark left by the previous (simulated-crashed)
            // session — this is `Allocator::new`'s entire seeding contract.
            let mut alloc = Allocator::new(durable_hw);
            for n in sizes {
                let range = alloc.allocate(n);
                for id in range {
                    prop_assert!(!used.contains(&id), "id {} reused across a simulated crash", id);
                    used.insert(id);
                }
            }
            durable_hw = alloc.high_water();
            // `alloc` is dropped here — the simulated crash.
        }
    }

    /// `assign_sorted` gives every item a distinct ID from a dense range, and items that share a
    /// signature (sorted, deduplicated term-ID list) land on a contiguous run of IDs.
    #[test]
    fn assign_sorted_groups_identical_signatures_contiguously(
        signatures in prop::collection::vec(prop::collection::vec(0u32..8, 0..5), 1..50),
    ) {
        let mut items: Vec<PendingItem> = signatures
            .iter()
            .enumerate()
            .map(|(i, sig)| PendingItem {
                external_id: format!("ext-{i:05}").into_bytes(),
                terms: sig.iter().map(|&t| TermId::new(t)).collect(),
                entity_id: None,
            })
            .collect();

        let mut alloc = Allocator::new(0);
        assign_sorted(&mut items, &mut alloc);

        let n = items.len() as u64;
        let mut ids: Vec<u64> = items
            .iter()
            .map(|it| it.entity_id.expect("assign_sorted must fill every entity_id").raw())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        prop_assert_eq!(ids, (0..n).collect::<Vec<_>>(), "ids are not a dense, distinct 0..n range");

        // Walk items in assigned-ID order; each time the (sorted, deduplicated) signature
        // changes, it must never recur — otherwise two runs of the same signature exist.
        let mut by_id: Vec<&PendingItem> = items.iter().collect();
        by_id.sort_by_key(|it| it.entity_id.unwrap().raw());

        let mut closed_signatures: HashSet<Vec<u32>> = HashSet::new();
        let mut current: Option<Vec<u32>> = None;
        for it in &by_id {
            let mut sig: Vec<u32> = it.terms.iter().map(|t| t.raw()).collect();
            sig.sort_unstable();
            sig.dedup();

            if current.as_ref() != Some(&sig) {
                if let Some(prev) = current.take() {
                    closed_signatures.insert(prev);
                }
                prop_assert!(
                    !closed_signatures.contains(&sig),
                    "signature {:?} reappeared after a different signature — not contiguous",
                    sig
                );
                current = Some(sig);
            }
        }
    }
}
