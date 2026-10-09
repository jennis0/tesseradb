//! The partition kind of the generated corpus: a single-valued attribute over every entity.
//!
//! [`crate::artifacts`]'s interval and scatter members overlap. An attribute predicate is the one
//! membership shape that cannot be tested by it: `membership = { attribute = "<field>" }` reads
//! one value column, so every entity must resolve to exactly one artifact.
//! [`Corpus::partition_artifact_of`] answers it directly from `(seed, layer, e)`, and
//! [`Corpus::partition_members`] is its exact inverse, closed form both ways.
//!
//! # `n` enters the population and nothing else
//!
//! The stride an artifact is built around ([`UNIT`]) is a constant, and the number of artifacts is
//! derived from it. The same seed at a smaller `n` gives this partition truncated: entity `e` is
//! in the same artifact at 10^4 as at 10^9. Per-layer variation lives in the jitter
//! (`partition_jitter_slots`), keyed on the layer.
//!
//! # A jittered stride
//!
//! A plain stride (`e / UNIT`) gives every artifact the same size, which tests no size skew. Each
//! artifact's size is instead `UNIT + delta`, `delta` drawn from a short keyed cycle ([`PERIOD`]
//! slots) with exactly one `+D` and one `-D` slot and the rest zero, so every full cycle's sizes
//! sum to exactly `PERIOD * UNIT`.
//!
//! Coverage is exact because the boundaries tile entity space: consecutive boundaries differ by
//! `UNIT + delta`, which lies in `{75, 100, 125}` and is always positive, and
//! `partition_boundary(0) = 0`, so `[boundary(a), boundary(a + 1))` for `a = 0, 1, 2, …` partitions
//! `0..∞` with no gap and no overlap.
//!
//! # The reverse direction is a bounded local search
//!
//! Because the drift from the plain stride is bounded (`|delta| <= UNIT / 4`), the artifact holding
//! entity `e` is always within one slot of `e / UNIT`, so [`Corpus::partition_artifact_of`] checks
//! only the three candidates around that estimate.

use crate::artifacts::layer_salt;
use crate::{keyed, Corpus, Grant};

const SALT_P_SLOT: u64 = crate::salt(b"prt-slt ");

/// Slots in the jitter's repeating cycle.
const PERIOD: u64 = 8;

/// The nominal, undithered size every artifact is built around.
const UNIT: u64 = 100;

impl Corpus {
    /// How many artifacts the partition layer holds: one past the artifact holding the last entity,
    /// and `0` for an empty corpus.
    pub fn partition_count(&self, layer: u64) -> u64 {
        match self.n() {
            0 => 0,
            n => self.partition_artifact_of(layer, n - 1) + 1,
        }
    }

    /// The two distinct cycle slots that hold `+D`/`-D`, fixed per layer, never per artifact.
    fn partition_jitter_slots(&self, layer: u64) -> (u64, u64) {
        let p1 = keyed(self.seed(), SALT_P_SLOT ^ layer_salt(layer, 0), layer) % PERIOD;
        let step = 1 + keyed(
            self.seed(),
            SALT_P_SLOT.wrapping_add(1) ^ layer_salt(layer, 0),
            layer,
        ) % (PERIOD - 1);
        let p2 = (p1 + step) % PERIOD;
        (p1, p2)
    }

    /// `delta(slot)`: `+D` at one cycle slot, `-D` at another, zero everywhere else.
    fn partition_delta(&self, layer: u64, slot: u64) -> i64 {
        let d = (UNIT / 4) as i64;
        let (p1, p2) = self.partition_jitter_slots(layer);
        if slot == p1 {
            d
        } else if slot == p2 {
            -d
        } else {
            0
        }
    }

    /// The prefix sum of [`Self::partition_delta`] over one cycle: zero at both ends.
    fn partition_cum_delta(&self, layer: u64, slots: u64) -> i64 {
        (0..slots).map(|i| self.partition_delta(layer, i)).sum()
    }

    /// Artifact `a`'s lower bound over entity space, exact and independent of `n`. `u128` because
    /// [`Self::partition_artifact_of`] is asked about the image of an arbitrary `fx_key`, so
    /// `a * UNIT` past the top of `u64` must be representable without saturating.
    fn partition_boundary(&self, layer: u64, a: u64) -> u128 {
        let base = u128::from(a) * u128::from(UNIT);
        (base as i128 + i128::from(self.partition_cum_delta(layer, a % PERIOD))).max(0) as u128
    }

    /// Artifact `a`'s interval `[lo, hi)` over the corpus: its boundaries, clipped to `n`.
    fn partition_interval(&self, layer: u64, a: u64) -> (u64, u64) {
        let n = u128::from(self.n());
        let lo = self.partition_boundary(layer, a).min(n) as u64;
        let hi = self.partition_boundary(layer, a.saturating_add(1)).min(n) as u64;
        (lo, hi.max(lo))
    }

    /// The members of artifact `a`: the forward direction, a contiguous run.
    pub fn partition_members(&self, layer: u64, a: u64) -> Vec<u64> {
        let (lo, hi) = self.partition_interval(layer, a);
        (lo..hi).collect()
    }

    /// The one artifact holding entity `e`: single-valued, defined for every `e`, including
    /// `e >= n` and on a corpus built with `n = 0`. O(1): checks the three candidates around the
    /// undithered stride's estimate (module doc).
    pub fn partition_artifact_of(&self, layer: u64, e: u64) -> u64 {
        let estimate = e / UNIT;
        let e = u128::from(e);
        for a in estimate.saturating_sub(1)..=estimate + 1 {
            let (lo, hi) = (
                self.partition_boundary(layer, a),
                self.partition_boundary(layer, a + 1),
            );
            if e >= lo && e < hi {
                return a;
            }
        }
        unreachable!("the boundaries tile entity space, so entity {e} is inside one of them")
    }

    /// The partition census: one O(n) pass, per-artifact visible counts.
    pub fn partition_artifact_census(&self, layer: u64, grant: &Grant) -> Vec<(u64, u64)> {
        self.bucket_census(grant, |e| [self.partition_artifact_of(layer, e)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{assert_census_counts_visible_members, corpus};

    /// Every entity belongs to exactly one artifact.
    #[test]
    fn every_entity_belongs_to_exactly_one_artifact() {
        let c = corpus(10_000);
        let count = c.partition_count(9);
        let mut owner = vec![u64::MAX; c.n() as usize];
        for a in 0..count {
            for e in c.partition_members(9, a) {
                assert_eq!(
                    owner[e as usize],
                    u64::MAX,
                    "entity {e} is claimed by two artifacts: {} and {a}",
                    owner[e as usize]
                );
                owner[e as usize] = a;
            }
        }
        assert!(
            owner.iter().all(|&a| a != u64::MAX),
            "some entity belongs to no artifact"
        );
        for e in 0..c.n() {
            assert_eq!(
                c.partition_artifact_of(9, e),
                owner[e as usize],
                "the reverse direction disagrees at entity {e}"
            );
        }
    }

    /// Sizes vary, and every artifact still holds at least one member.
    #[test]
    fn artifact_sizes_are_not_all_equal() {
        let c = corpus(20_000);
        let count = c.partition_count(4);
        let sizes: Vec<usize> = (0..count)
            .map(|a| c.partition_members(4, a).len())
            .collect();
        assert!(
            sizes.iter().all(|&s| s > 0),
            "an artifact is empty: {sizes:?}"
        );
        assert!(
            sizes.iter().min() != sizes.iter().max(),
            "every artifact is the same size, so the jitter is untested: {sizes:?}"
        );
    }

    /// The jitter's shape, which two cycle slots widen and narrow a stride, is a property of
    /// `(seed, layer)` alone and does not move with `n`.
    #[test]
    fn the_jitter_shape_does_not_depend_on_the_corpus_size() {
        let small = corpus(4_000);
        let large = corpus(400_000);
        for layer in 0..6 {
            assert_eq!(
                small.partition_jitter_slots(layer),
                large.partition_jitter_slots(layer),
                "layer {layer}'s jitter shape moved with n"
            );
        }
    }

    #[test]
    fn the_census_counts_each_artifacts_visible_members() {
        let c = corpus(10_000);
        let grant = Grant::parse("5,6,7,8").unwrap();
        assert_census_counts_visible_members(
            &c,
            &grant,
            c.partition_artifact_census(2, &grant),
            0..c.partition_count(2),
            |a| c.partition_members(2, a),
        );
    }
}
