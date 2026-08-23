//! The partition arm of the generated corpus: a single-valued attribute over every entity.
//!
//! [`crate::artifacts`]'s interval and scatter arms overlap **deliberately** — that is the case a
//! model permitting multi-membership needs a fixture for — and an attribute predicate is the one
//! membership shape that cannot be tested by it: `membership = { attribute = "<field>" }` reads one
//! value column, so every entity must resolve to exactly one artifact. This arm is that partition:
//! [`Corpus::partition_artifact_of`] answers it directly from `(seed, layer, e)`, and
//! [`Corpus::partition_members`] is its exact inverse, closed form both ways.
//!
//! # A jittered stride, with the jitter's drift cancelled rather than clipped away
//!
//! A plain stride (`e / (n / count)`) gives every artifact the same size, which tests none of the
//! size-skew a real attribute value's population has. So each artifact's size is `unit + delta`,
//! `delta` drawn from a short keyed cycle ([`PERIOD`] slots) rather than per-artifact, with exactly
//! one `+D` and one `-D` slot and the rest zero — chosen so **every full cycle's sizes sum to
//! exactly `PERIOD * unit`, by construction**, not by rounding. That is what keeps the partition
//! exact at any *a*: `partition_boundary(a)` is `a * unit` plus the *within-cycle* prefix sum of
//! deltas, so a boundary never drifts from the stride it was built around, and the closed form does
//! not need to track a running total across the whole corpus to stay exact.
//!
//! # The reverse direction is a bounded local search, not a scan
//!
//! Because the drift from the plain stride is bounded (`|delta| <= unit / 4`), the artifact holding
//! entity *e* is always within one slot of `e / unit`: [`Corpus::partition_artifact_of`] checks a
//! handful of candidates around that estimate and returns the one whose interval contains *e* —
//! O(1), never a walk over *n* or over `count`.

use crate::artifacts::layer_salt;
use crate::{keyed, Corpus, Grant};

const SALT_P_COUNT: u64 = crate::salt(b"prt-cnt ");
const SALT_P_SLOT: u64 = crate::salt(b"prt-slt ");

/// Slots in the jitter's repeating cycle. Small and fixed: the local search in
/// [`Corpus::partition_artifact_of`] checks a window of this width around its estimate, and the
/// cycle's own drift bound (`unit / 4`) is what keeps that window a handful of candidates rather
/// than a scan.
const PERIOD: u64 = 8;

impl Corpus {
    /// How many artifacts the partition layer holds. Scaled with `n` on the same 1-in-100 ratio
    /// [`crate::artifacts::Corpus::artifacts_in`] uses, and independent of it otherwise — this is
    /// the one place `n` is licensed to enter (see that method's doc for why).
    pub fn partition_count(&self, layer: u64) -> u64 {
        let scaled = self.n() / 100;
        let by_layer = keyed(self.seed(), SALT_P_COUNT, layer) % 4;
        scaled.saturating_sub(by_layer).max(2)
    }

    /// The nominal (undithered) size every artifact is built around.
    fn partition_unit(&self, count: u64) -> u64 {
        (self.n() / count.max(1)).max(4)
    }

    /// The two distinct cycle slots that carry `+D`/`-D` — fixed per layer, never per artifact, so
    /// every cycle of [`PERIOD`] artifacts repeats the identical jitter shape.
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

    /// `delta(slot)`: `+D` at one cycle slot, `-D` at another, zero everywhere else — the two
    /// always distinct ([`Self::partition_jitter_slots`]), so every full cycle's deltas sum to
    /// exactly zero.
    fn partition_delta(&self, layer: u64, unit: u64, slot: u64) -> i64 {
        let d = (unit / 4) as i64;
        if d == 0 {
            return 0;
        }
        let (p1, p2) = self.partition_jitter_slots(layer);
        if slot == p1 {
            d
        } else if slot == p2 {
            -d
        } else {
            0
        }
    }

    /// The prefix sum of [`Self::partition_delta`] over one cycle, `cum(0) = 0 .. cum(PERIOD) = 0`
    /// — zero at both ends because the two nonzero slots cancel exactly.
    fn partition_cum_delta(&self, layer: u64, unit: u64, slots: u64) -> i64 {
        (0..slots)
            .map(|i| self.partition_delta(layer, unit, i))
            .sum()
    }

    /// Artifact `a`'s lower bound, exact: `a * unit` plus the within-cycle prefix sum at
    /// `a mod PERIOD`. Full cycles contribute nothing beyond `a * unit` because a cycle's deltas
    /// sum to zero — see the module doc for why that makes this exact rather than merely close.
    fn partition_boundary(&self, layer: u64, unit: u64, a: u64) -> u64 {
        let base = (a * unit) as i64;
        (base + self.partition_cum_delta(layer, unit, a % PERIOD)).max(0) as u64
    }

    /// Artifact `a`'s interval `[lo, hi)` over entity space, clipped to the corpus — the last
    /// artifact's `hi` is forced to `n` regardless of the jittered boundary, absorbing whatever the
    /// stride estimate over- or under-shoots, the same clipping rule
    /// [`crate::artifacts::Corpus::artifact_members`]'s interval half uses.
    fn partition_interval(&self, layer: u64, count: u64, unit: u64, a: u64) -> (u64, u64) {
        if a >= count {
            return (self.n(), self.n());
        }
        let lo = self.partition_boundary(layer, unit, a).min(self.n());
        let hi = if a + 1 == count {
            self.n()
        } else {
            self.partition_boundary(layer, unit, a + 1).min(self.n())
        };
        (lo, hi.max(lo))
    }

    /// The members of artifact `a` — the forward direction, a contiguous run because this arm
    /// partitions rather than overlaps.
    pub fn partition_members(&self, layer: u64, a: u64) -> Vec<u64> {
        let count = self.partition_count(layer);
        let unit = self.partition_unit(count);
        let (lo, hi) = self.partition_interval(layer, count, unit, a);
        (lo..hi).collect()
    }

    /// The one artifact holding entity `e` — single-valued, unlike the flat arm's
    /// `artifacts_holding`, because a predicate over one value column can only ever name one.
    ///
    /// O(1): estimates `a` from the undithered stride and checks a small window around it, which
    /// suffices because the jitter's drift is bounded to `unit / 4` either way of that estimate
    /// (module doc). Defined for `e < n`; clamped to the last artifact past the corpus's own end,
    /// matching [`Self::partition_interval`]'s clip.
    pub fn partition_artifact_of(&self, layer: u64, e: u64) -> u64 {
        let count = self.partition_count(layer);
        let unit = self.partition_unit(count);
        let estimate = e / unit;
        let lo_candidate = estimate.saturating_sub(2);
        let hi_candidate = (estimate + 2).min(count - 1);
        for a in lo_candidate..=hi_candidate {
            let (lo, hi) = self.partition_interval(layer, count, unit, a);
            if e >= lo && e < hi {
                return a;
            }
        }
        count - 1
    }

    /// The partition census: one O(*n*) pass, per-artifact visible counts — see
    /// [`crate::Corpus::bucket_census`].
    pub fn partition_artifact_census(&self, layer: u64, grant: &Grant) -> Vec<(u64, u64)> {
        self.bucket_census(grant, |e| vec![self.partition_artifact_of(layer, e)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_spatial::Bounds;

    fn corpus(n: u64) -> Corpus {
        Corpus::new(
            0x5EED,
            n,
            Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
        )
        .unwrap()
    }

    /// **Single-valued, exhaustively**: every entity belongs to exactly one artifact, the
    /// partition's whole point — checked by building the forward relation once and comparing every
    /// entity's reverse answer against it, the same property the flat arm's own census test rests
    /// on.
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

    /// Sizes vary — the jitter is not a no-op — while every artifact still holds at least one
    /// member (the drift is bounded well short of emptying one out at this scale).
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

    /// The jitter's *shape* — which two cycle slots widen and narrow a stride — is a property of
    /// `(seed, layer)` alone and does not move with `n`. Population legitimately scales with `n`
    /// (the one licensed use, [`Corpus::partition_count`]'s doc), so entity-to-artifact assignment
    /// itself is not claimed stable here — only the rule that produces it, the same distinction
    /// the flat arm's declared-shape test draws.
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

    /// The census oracle agrees with the materialised relation, by brute force.
    #[test]
    fn the_census_agrees_with_partition_members_by_brute_force() {
        let c = corpus(10_000);
        let grant = crate::Grant::parse("5,6,7,8").unwrap();
        let count = c.partition_count(2);
        let mut expected = std::collections::BTreeMap::new();
        for a in 0..count {
            let visible = c
                .partition_members(2, a)
                .into_iter()
                .filter(|e| c.visible(*e, &grant))
                .count() as u64;
            if visible > 0 {
                expected.insert(a, visible);
            }
        }
        let census = c.partition_artifact_census(2, &grant);
        let got: std::collections::BTreeMap<u64, u64> = census.into_iter().collect();
        assert_eq!(
            got, expected,
            "the census and the brute-force count disagree"
        );
    }
}
