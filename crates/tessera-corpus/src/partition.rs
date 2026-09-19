//! The partition arm of the generated corpus: a single-valued attribute over every entity.
//!
//! [`crate::artifacts`]'s interval and scatter arms overlap **deliberately** — that is the case a
//! model permitting multi-membership needs a fixture for — and an attribute predicate is the one
//! membership shape that cannot be tested by it: `membership = { attribute = "<field>" }` reads one
//! value column, so every entity must resolve to exactly one artifact. This arm is that partition:
//! [`Corpus::partition_artifact_of`] answers it directly from `(seed, layer, e)`, and
//! [`Corpus::partition_members`] is its exact inverse, closed form both ways.
//!
//! # *n* enters the population and nothing else
//!
//! The stride an artifact is built around ([`UNIT`]) is a **constant**, and the number of artifacts
//! is derived from it: `partition_count` is however many strides the corpus reaches. That is the
//! direction [`crate::artifacts::Corpus::artifacts_in`] states as the rule — population scales with
//! the corpus, every property *of* an artifact is independent of it — so the same seed at a smaller
//! *n* is this arm **truncated**: entity *e* is in the same artifact at 10⁴ as at 10⁹. Deriving the
//! stride from a count that was itself derived from *n* inverts that and re-assigns entities as the
//! corpus grows, which is what this arm did until 2026-08-30 and what
//! [`Corpus::partition_artifact_of`]'s claim to be a function of `(seed, layer, e)` could not
//! survive.
//!
//! Per-layer variation therefore lives in the jitter (`partition_jitter_slots`), which is
//! keyed on the layer, rather than in the count, which no longer has room to carry it.
//!
//! # A jittered stride, with the jitter's drift cancelled rather than clipped away
//!
//! A plain stride (`e / UNIT`) gives every artifact the same size, which tests none of the
//! size-skew a real attribute value's population has. So each artifact's size is `UNIT + delta`,
//! `delta` drawn from a short keyed cycle ([`PERIOD`] slots) rather than per-artifact, with exactly
//! one `+D` and one `-D` slot and the rest zero — chosen so **every full cycle's sizes sum to
//! exactly `PERIOD * UNIT`, by construction**, not by rounding. That is what keeps the partition
//! exact at any *a*: `partition_boundary(a)` is `a * UNIT` plus the *within-cycle* prefix sum of
//! deltas, so a boundary never drifts from the stride it was built around, and the closed form does
//! not need to track a running total across the whole corpus to stay exact.
//!
//! **Coverage is exact because the boundaries tile the whole of entity space.** Consecutive
//! boundaries differ by `UNIT + delta`, which lies in `{75, 100, 125}` and is therefore always
//! positive, and `partition_boundary(0) = 0`; so `[boundary(a), boundary(a + 1))` for
//! `a = 0, 1, 2, …` partitions `0..∞` with no gap and no overlap, before *n* is mentioned at all.
//! The corpus is that tiling intersected with `0..n`, and `partition_count` is one past the
//! artifact holding `n - 1` — so a tail artifact smaller than `UNIT` is ordinary, and an empty one
//! cannot arise.
//!
//! # The reverse direction is a bounded local search, not a scan
//!
//! Because the drift from the plain stride is bounded (`|delta| <= UNIT / 4`), the artifact holding
//! entity *e* is always within one slot of `e / UNIT`: [`Corpus::partition_artifact_of`] checks the
//! three candidates around that estimate and returns the one whose interval contains *e* — O(1),
//! never a walk over *n* or over `count`. The bound is tight rather than generous: `e` lies in
//! artifact *a* only where `a * UNIT - D <= e < (a + 1) * UNIT + D` with `D = UNIT / 4`, and
//! dividing through by `UNIT` puts `e / UNIT` in `{a - 1, a, a + 1}`.

use crate::artifacts::layer_salt;
use crate::{keyed, Corpus, Grant};

const SALT_P_SLOT: u64 = crate::salt(b"prt-slt ");

/// Slots in the jitter's repeating cycle. Small and fixed: the local search in
/// [`Corpus::partition_artifact_of`] checks a window around its estimate, and the cycle's own drift
/// bound (`UNIT / 4`) is what keeps that window three candidates rather than a scan.
const PERIOD: u64 = 8;

/// The nominal (undithered) size every artifact is built around — **a constant, and the property
/// that makes this arm truncate**. A hundred members to an artifact, the same ratio
/// [`crate::artifacts::Corpus::artifacts_in`] sizes the flat arm's membership from (10⁹ rows over
/// 10⁷ artifacts).
///
/// It must stay at least `8` for the drift bound `D = UNIT / 4` to be nonzero and the jitter to be
/// visible at all; the reverse search's three-candidate window is derived from that same `D` and
/// holds for any value.
const UNIT: u64 = 100;

impl Corpus {
    /// How many artifacts the partition layer holds: one past the artifact holding the last entity,
    /// and `0` for an empty corpus.
    ///
    /// **This is the only place this arm lets *n* in** — the population scales with the corpus,
    /// which is the licence [`crate::artifacts::Corpus::artifacts_in`] states and this arm now
    /// keeps, while the stride each artifact is built around is `UNIT`, a constant. Because the
    /// last artifact is the one holding `n - 1`, it is never empty.
    pub fn partition_count(&self, layer: u64) -> u64 {
        match self.n() {
            0 => 0,
            n => self.partition_artifact_of(layer, n - 1) + 1,
        }
    }

    /// The two distinct cycle slots that carry `+D`/`-D` — fixed per layer, never per artifact, so
    /// every cycle of [`PERIOD`] artifacts repeats the identical jitter shape. This is the whole of
    /// the arm's per-layer variation: the count no longer carries any, because it is derived.
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

    /// The prefix sum of [`Self::partition_delta`] over one cycle, `cum(0) = 0 .. cum(PERIOD) = 0`
    /// — zero at both ends because the two nonzero slots cancel exactly.
    fn partition_cum_delta(&self, layer: u64, slots: u64) -> i64 {
        (0..slots).map(|i| self.partition_delta(layer, i)).sum()
    }

    /// Artifact `a`'s lower bound over entity space, exact and independent of *n*: `a * UNIT` plus
    /// the within-cycle prefix sum at `a mod PERIOD`. Full cycles contribute nothing beyond
    /// `a * UNIT` because a cycle's deltas sum to zero — see the module doc for why that makes this
    /// exact rather than merely close.
    ///
    /// `u128` because entity space here is the whole of `u64` rather than `0..n`:
    /// [`Self::partition_artifact_of`] is asked about the image of an arbitrary `fx_key`
    /// (correctness-suite §12.1), so `a * UNIT` for the artifact past the top of the range must be
    /// representable rather than saturated — a saturated top boundary would leave the very last
    /// entities in no artifact at all.
    fn partition_boundary(&self, layer: u64, a: u64) -> u128 {
        let base = u128::from(a) * u128::from(UNIT);
        (base as i128 + i128::from(self.partition_cum_delta(layer, a % PERIOD))).max(0) as u128
    }

    /// Artifact `a`'s interval `[lo, hi)` over the corpus: its boundaries, clipped to `n`. The
    /// clip is the truncation — an artifact past the corpus's end comes back empty, and the one
    /// holding `n - 1` comes back short — never a re-assignment of the entities before it.
    fn partition_interval(&self, layer: u64, a: u64) -> (u64, u64) {
        let n = u128::from(self.n());
        let lo = self.partition_boundary(layer, a).min(n) as u64;
        let hi = self.partition_boundary(layer, a.saturating_add(1)).min(n) as u64;
        (lo, hi.max(lo))
    }

    /// The members of artifact `a` — the forward direction, a contiguous run because this arm
    /// partitions rather than overlaps.
    pub fn partition_members(&self, layer: u64, a: u64) -> Vec<u64> {
        let (lo, hi) = self.partition_interval(layer, a);
        (lo..hi).collect()
    }

    /// The one artifact holding entity `e` — single-valued, unlike the flat arm's
    /// `artifacts_holding`, because a predicate over one value column can only ever name one.
    ///
    /// **A function of `(seed, layer, e)` alone**: defined for every `e`, including `e >= n` and on
    /// a corpus built with `n = 0`, which is how `tessera corpus items` answers for a column it
    /// never sizes (correctness-suite §12.1).
    ///
    /// O(1): estimates `a` from the undithered stride and checks the three candidates around it,
    /// which suffices because the jitter's drift is bounded to `UNIT / 4` either way of that
    /// estimate (module doc).
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

    /// The partition census: one O(*n*) pass, per-artifact visible counts — see
    /// [`crate::Corpus::bucket_census`].
    pub fn partition_artifact_census(&self, layer: u64, grant: &Grant) -> Vec<(u64, u64)> {
        self.bucket_census(grant, |e| [self.partition_artifact_of(layer, e)])
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
    /// `(seed, layer)` alone and does not move with `n`. It is the rule that produces the
    /// assignment; that the assignment *itself* is stable across corpus sizes is the stronger
    /// property, and it is pinned in `tests/generator_props.rs`.
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
