//! The artifact kind of the generated corpus: annotations as a function of `(seed, layer, level, a)`.
//!
//! An item's properties need one direction: given `e`, what is it. An artifact needs two: the
//! members of artifact `a`, and the artifacts holding entity `e`. Both are closed form here, in
//! constant time, with no table and no I/O, which is what lets the check run at sizes where the
//! expected answer cannot be stored.
//!
//! # Membership has two parts
//!
//! A keyed interval, anchored on a regular stride and given a keyed length that may run past its
//! own block. The run past the block is what makes artifacts within a level overlap. The reverse
//! direction stays closed form because the overrun is bounded: an entity in block `b` can only be
//! held by artifacts `b - MAX_SPILL ..= b`.
//!
//! A keyed scatter, drawn through a keyed bijection over `[0, n)` instead of by sampling: a
//! sampled scatter is closed form forwards and a scan backwards. The scatter of artifact `a` is
//! the images of a contiguous run of indices, and the artifact that scattered `e` is found by
//! inverting `e` and dividing.
//!
//! # Cases the generator always includes
//!
//! - Artifact 0 of every level holds exactly one member.
//! - Artifact 1 of every level has no interval, so all of its members are scattered.
//! - Level 1 of every layer is empty.
//! - The own-term flag and the existence criterion are cycled independently, so all four
//!   combinations occur at every size.
//! - Generating sets are subsets of membership, about a hundred members each.

use crate::{keyed, mix64, salt, Corpus, Grant};

const SALT_A_INTERVAL: u64 = salt(b"art-ivl ");
const SALT_A_LENGTH: u64 = salt(b"art-len ");
const SALT_A_SCATTER: u64 = salt(b"art-scat");
const SALT_A_GEN: u64 = salt(b"art-gen ");
const SALT_A_FEISTEL: u64 = salt(b"art-fst ");

/// The most blocks an artifact's interval may extend past its own block.
const MAX_SPILL: u64 = 2;

/// Members of the scatter half, per artifact.
const SCATTER_PER_ARTIFACT: u64 = 8;

/// Members of a generating set, before it is clipped to the artifact's own membership.
const GENERATING_SET_TARGET: u64 = 100;

/// Which level of every layer is empty.
pub const EMPTY_LEVEL: u32 = 1;

/// One artifact's declared shape: everything about it that is not its membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactShape {
    /// Which term this artifact has, where its layer's `artifact_visibility` names a field.
    /// `None` where the artifact has none.
    pub own_term: Option<u32>,
    /// The existence criterion this artifact is tested against, as an absolute count. `None`
    /// where its layer declares none.
    pub min_visible: Option<u64>,
}

impl Corpus {
    /// How many artifacts level `level` of `layer` holds. Population scales with the corpus, so
    /// the same seed at a smaller `n` gives this level truncated.
    pub fn artifacts_in(&self, layer: u64, level: u32) -> u64 {
        if level == EMPTY_LEVEL {
            return 0;
        }
        // A hundred members to an artifact at the interval's stride.
        let scaled = self.n() / 100;
        let by_layer = keyed(self.seed(), SALT_A_INTERVAL, layer) % 4;
        scaled.saturating_sub(by_layer).max(2)
    }

    /// The members of artifact `a`: the closed-form forward direction. Ascending and deduplicated.
    pub fn artifact_members(&self, layer: u64, level: u32, a: u64) -> Vec<u64> {
        let mut members = Vec::new();
        let (lo, hi) = self.artifact_interval(layer, level, a);
        members.extend(lo..hi);
        for j in 0..SCATTER_PER_ARTIFACT {
            let index = a.saturating_mul(SCATTER_PER_ARTIFACT) + j;
            if index >= self.n() {
                break;
            }
            members.push(self.shuffle(SALT_A_SCATTER ^ layer_salt(layer, level), index));
        }
        members.sort_unstable();
        members.dedup();
        members
    }

    /// Every artifact of `(layer, level)` holding entity `e`: the closed-form reverse direction.
    /// Ascending. Constant time.
    pub fn artifacts_holding(&self, layer: u64, level: u32, e: u64) -> Vec<u64> {
        let count = self.artifacts_in(layer, level);
        if count == 0 || e >= self.n() {
            return Vec::new();
        }
        let mut holders = Vec::new();

        let stride = self.stride(count);
        let block = e / stride;
        let first = block.saturating_sub(MAX_SPILL);
        for a in first..=block.min(count - 1) {
            let (lo, hi) = self.artifact_interval(layer, level, a);
            if e >= lo && e < hi {
                holders.push(a);
            }
        }

        // The scatter, inverted: if `e` is the image of an index inside the scattered run, the
        // artifact that scattered it is that index divided by the run length. If it is not, no
        // artifact scattered it.
        let index = self.unshuffle(SALT_A_SCATTER ^ layer_salt(layer, level), e);
        if index < count.saturating_mul(SCATTER_PER_ARTIFACT) {
            let a = index / SCATTER_PER_ARTIFACT;
            if a < count && !holders.contains(&a) {
                holders.push(a);
            }
        }

        holders.sort_unstable();
        holders
    }

    /// The generating set of artifact `a`: about a hundred of its own members, always a subset of
    /// the membership.
    pub fn artifact_generating_set(&self, layer: u64, level: u32, a: u64) -> Vec<u64> {
        let members = self.artifact_members(layer, level, a);
        if members.is_empty() {
            return Vec::new();
        }
        let want = GENERATING_SET_TARGET.min(members.len() as u64) as usize;
        let mut set: Vec<u64> = (0..want as u64)
            .map(|j| {
                let pick = keyed(
                    self.seed(),
                    SALT_A_GEN ^ layer_salt(layer, level),
                    a ^ (j << 32),
                );
                members[(pick % members.len() as u64) as usize]
            })
            .collect();
        set.sort_unstable();
        set.dedup();
        set
    }

    /// The declared shape of artifact `a`: the two conditions, cycled so all four combinations
    /// occur at every size.
    pub fn artifact_shape(&self, layer: u64, level: u32, a: u64) -> ArtifactShape {
        let cycle = a % 4;
        ArtifactShape {
            own_term: (cycle & 1 == 1).then(|| {
                (keyed(self.seed(), SALT_A_GEN ^ layer_salt(layer, level), a)
                    % u64::from(self.term_space())) as u32
            }),
            min_visible: (cycle & 2 == 2).then_some(4),
        }
    }

    /// The artifact census over this layer and level: how many of each artifact's members `grant`
    /// sees. An artifact this grant sees nothing of is absent, never a zero.
    pub fn flat_artifact_census(&self, layer: u64, level: u32, grant: &Grant) -> Vec<(u64, u64)> {
        self.bucket_census(grant, |e| self.artifacts_holding(layer, level, e))
    }

    /// The stride the intervals are anchored on.
    fn stride(&self, count: u64) -> u64 {
        (self.n() / count.max(1)).max(1)
    }

    /// Artifact `a`'s interval, `[lo, hi)`, clipped to the corpus. Artifact 0 holds exactly one
    /// member and artifact 1 holds none by interval.
    fn artifact_interval(&self, layer: u64, level: u32, a: u64) -> (u64, u64) {
        let count = self.artifacts_in(layer, level);
        if count == 0 || a >= count {
            return (0, 0);
        }
        let stride = self.stride(count);
        let lo = (a.saturating_mul(stride)).min(self.n());
        if a == 0 {
            return (lo, (lo + 1).min(self.n()));
        }
        if a == 1 {
            // Its members are the scatter alone.
            return (lo, lo);
        }
        // A keyed length between one and `MAX_SPILL + 1` strides: the overrun is bounded so the
        // reverse direction stays a short walk.
        let spill =
            keyed(self.seed(), SALT_A_LENGTH ^ layer_salt(layer, level), a) % (MAX_SPILL + 1);
        let len = stride.saturating_mul(spill + 1);
        (lo, (lo + len).min(self.n()))
    }

    /// A keyed bijection on `[0, n)`, by cycle-walking a four-round Feistel over the smallest
    /// even-bit domain covering `n`. An image outside `[0, n)` is fed back in.
    fn shuffle(&self, key: u64, x: u64) -> u64 {
        self.walk(key, x, false)
    }

    /// The exact inverse of [`Self::shuffle`], walking the same cycle backwards.
    fn unshuffle(&self, key: u64, y: u64) -> u64 {
        self.walk(key, y, true)
    }

    fn walk(&self, key: u64, start: u64, reverse: bool) -> u64 {
        let (half, mask) = self.feistel_shape();
        let mut v = start;
        loop {
            v = feistel(self.seed() ^ SALT_A_FEISTEL ^ key, half, mask, v, reverse);
            if v < self.n() {
                return v;
            }
        }
    }

    /// Half-width in bits, and the mask of one half: the smallest even width covering `n`.
    fn feistel_shape(&self) -> (u32, u64) {
        let bits = (64 - self.n().saturating_sub(1).leading_zeros()).max(2);
        let half = bits.div_ceil(2);
        (half, (1u64 << half) - 1)
    }
}

/// One layer-and-level's key material, so two levels of one layer are as unrelated as two layers.
/// `pub(crate)` because [`crate::partition`] and [`crate::boundary`] key their own kinds of
/// artifact by the same `(layer, level)` pair.
pub(crate) fn layer_salt(layer: u64, level: u32) -> u64 {
    mix64(layer.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(level))
}

/// Four Feistel rounds over `2 * half` bits, forwards or in reverse. Three is the textbook minimum
/// for a pseudorandom permutation; the fourth mixes visibly better at the small widths a fixture
/// uses.
fn feistel(key: u64, half: u32, mask: u64, v: u64, reverse: bool) -> u64 {
    let mut l = (v >> half) & mask;
    let mut r = v & mask;
    for step in 0..4u64 {
        let round = if reverse { 3 - step } else { step };
        let f = |half_value: u64| mix64(key ^ mix64(round + 1) ^ half_value) & mask;
        // A round is (l, r) -> (r, l ^ f(r)); its inverse is (l, r) -> (r ^ f(l), l).
        (l, r) = if reverse {
            (r ^ f(l), l)
        } else {
            (r, l ^ f(r))
        };
    }
    (l << half) | r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{assert_census_counts_visible_members, corpus};

    /// `artifacts_holding` is the inverse of `artifact_members` for every entity.
    #[test]
    fn the_two_directions_agree_over_every_entity() {
        let c = corpus(4_000);
        for level in [0u32, 2] {
            let count = c.artifacts_in(7, level);
            let mut expected: Vec<Vec<u64>> = vec![Vec::new(); c.n() as usize];
            for a in 0..count {
                for e in c.artifact_members(7, level, a) {
                    expected[e as usize].push(a);
                }
            }
            for (e, holders) in expected.iter_mut().enumerate() {
                holders.sort_unstable();
                holders.dedup();
                assert_eq!(
                    c.artifacts_holding(7, level, e as u64),
                    *holders,
                    "the reverse direction disagrees at entity {e}"
                );
            }
        }
    }

    /// Every property of an artifact is independent of the corpus size, so a smaller `n` gives the
    /// same level truncated.
    #[test]
    fn an_artifacts_membership_does_not_depend_on_the_corpus_size() {
        let small = corpus(4_000);
        let large = corpus(40_000);
        for a in 0..8 {
            assert_eq!(
                small.artifact_shape(3, 0, a),
                large.artifact_shape(3, 0, a),
                "artifact {a}'s declared shape moved with n"
            );
        }
    }

    /// The bijection is one, over the whole corpus, and its inverse is exact.
    #[test]
    fn the_scatter_permutation_is_a_bijection() {
        let c = corpus(1_000);
        let mut seen = vec![false; c.n() as usize];
        for x in 0..c.n() {
            let y = c.shuffle(0x1234, x);
            assert!(y < c.n(), "the walk left the corpus at {x}");
            assert!(!seen[y as usize], "two entities share an image at {x}");
            seen[y as usize] = true;
            assert_eq!(c.unshuffle(0x1234, y), x, "the inverse is not one at {x}");
        }
    }

    /// The fixed cases are present at every size.
    #[test]
    fn the_planted_shapes_are_present_at_every_size() {
        for n in [1_000u64, 10_000, 100_000] {
            let c = corpus(n);
            assert_eq!(
                c.artifacts_in(1, EMPTY_LEVEL),
                0,
                "the empty level has artifacts at n={n}"
            );
            assert_eq!(
                c.artifact_members(1, 0, 0).len(),
                1 + SCATTER_PER_ARTIFACT as usize,
                "artifact 0 is not the single-interval-member case at n={n}"
            );
            assert_eq!(
                c.artifact_members(1, 0, 1).len(),
                SCATTER_PER_ARTIFACT as usize,
                "artifact 1 is not the scatter-only case at n={n}"
            );
            // All four combinations, in the first four artifacts.
            let shapes: Vec<_> = (0..4).map(|a| c.artifact_shape(1, 0, a)).collect();
            assert_eq!(
                shapes
                    .iter()
                    .filter(|s| s.own_term.is_some() && s.min_visible.is_some())
                    .count(),
                1,
                "the both-conditions case is missing at n={n}"
            );
            assert_eq!(
                shapes
                    .iter()
                    .filter(|s| s.own_term.is_none() && s.min_visible.is_none())
                    .count(),
                1,
                "the neither-condition case is missing at n={n}"
            );
        }
    }

    /// Artifacts within a level overlap, which the model permits.
    #[test]
    fn artifacts_within_a_level_overlap() {
        let c = corpus(4_000);
        let shared = (0..c.n())
            .filter(|e| c.artifacts_holding(2, 0, *e).len() > 1)
            .count();
        assert!(shared > 0, "no entity is in two artifacts");
    }

    /// A generating set is drawn from the artifact's own members, always.
    #[test]
    fn a_generating_set_is_a_subset_of_the_membership() {
        let c = corpus(4_000);
        for a in 0..6 {
            let members = c.artifact_members(5, 0, a);
            for source in c.artifact_generating_set(5, 0, a) {
                assert!(
                    members.contains(&source),
                    "artifact {a}'s generating set names {source}, which is not a member"
                );
            }
        }
    }

    #[test]
    fn the_census_counts_each_artifacts_visible_members() {
        let c = corpus(10_000);
        let grant = Grant::parse("0,1,2,3").unwrap();
        assert_census_counts_visible_members(
            &c,
            &grant,
            c.flat_artifact_census(6, 0, &grant),
            0..c.artifacts_in(6, 0),
            |a| c.artifact_members(6, 0, a),
        );
    }
}
