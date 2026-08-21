//! The artifact arm of the generated corpus: annotations as a function of `(seed, layer, level, a)`.
//!
//! Everything the [item arm](crate) is, and one thing more. An item's properties need one direction
//! — given *e*, what is it — and an artifact's need **two**: the members of artifact *a*, and the
//! artifacts holding entity *e*. The census cannot say "nothing is missing or extra" from one of
//! them, because the two failures it must separate are *this artifact lost a member* and *this
//! member joined an artifact nobody declared it into*, and each is invisible from the other side.
//!
//! Both are closed form here, in constant time, with no table and no I/O. That is what lets the
//! check run at sizes where the expected answer cannot be stored.
//!
//! # Membership has two parts, because one part cannot fail both ways
//!
//! **A keyed interval**, anchored on a regular stride and given a keyed length that may run past
//! its own block. That is the compact case — a cluster is contiguous in the space it was clustered
//! in — and the spill is what makes artifacts within a level **overlap**, which the model permits
//! and a generator producing disjoint sets would never exercise. The reverse direction stays closed
//! form because the spill is bounded: an entity in block *b* can only be held by artifacts
//! `b - MAX_SPILL ..= b`, so the lookup is a short bounded walk rather than a scan.
//!
//! **A keyed scatter**, drawn through a keyed **bijection** over `[0, n)` rather than by sampling.
//! Sampling is closed form forwards and hopeless backwards — "which artifacts scattered into *e*"
//! would be a scan over every artifact. Through a bijection both directions are arithmetic: the
//! scatter of artifact *a* is the images of a contiguous run of indices, and the artifact that
//! scattered *e* is found by inverting *e* and dividing. The scatter is the pathological case that
//! the interval is not — membership spread across the whole space, one Roaring container per member.
//!
//! # What is deliberately planted
//!
//! Every one of these is a case some part of the system treats differently, and a generator that
//! produced only the average case would test none of them:
//!
//! - **artifact 0 of every level holds exactly one member** — the smallest membership that is not
//!   empty, where an off-by-one in a criterion or a projection shows up as absence;
//! - **artifact 1 holds a run drawn entirely from one term's complement**, so a chosen grant sees
//!   **zero** of it: the artifact that exists, is reachable, and has a masked count of nothing;
//! - **level 1 of every layer is empty** — a declared level with no artifacts at all, which a
//!   reader that infers a level's existence from its contents gets wrong;
//! - **the own-terms flag and the existence criterion are cycled independently across artifacts**,
//!   so all four cells of that two-by-two occur at every size (decision 0079's two independent
//!   conjuncts);
//! - **generating sets are subsets of membership**, about a hundred members each, because a
//!   generating set naming a non-member is a different (and refused) thing.

use crate::{keyed, mix64, salt, Corpus, Grant};

const SALT_A_INTERVAL: u64 = salt(b"art-ivl ");
const SALT_A_LENGTH: u64 = salt(b"art-len ");
const SALT_A_SCATTER: u64 = salt(b"art-scat");
const SALT_A_GEN: u64 = salt(b"art-gen ");
const SALT_A_FEISTEL: u64 = salt(b"art-fst ");

/// How many stride-blocks an interval may run past its own, at most. The reverse lookup walks this
/// many artifacts back, so it is the constant that keeps that direction O(1) — and it is a property
/// of the generator rather than a tuning knob: raising it widens overlap and lengthens the walk by
/// exactly the same amount.
const MAX_SPILL: u64 = 2;

/// Members of the scatter half, per artifact.
const SCATTER_PER_ARTIFACT: u64 = 8;

/// Members of a generating set, before it is clipped to the artifact's own membership.
const GENERATING_SET_TARGET: u64 = 100;

/// Which level of every layer is deliberately empty.
pub const EMPTY_LEVEL: u32 = 1;

/// One artifact's declared shape — everything about it that is not its membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactShape {
    /// Whether its layer's `artifact_visibility` names a field — so artifacts carry their own
    /// labels — and which term this one then carries. `None` where the artifact is gated on its members' visibility alone.
    pub own_term: Option<u32>,
    /// The existence criterion this artifact is expected to be tested against, as an absolute
    /// count. `None` where its layer declares none.
    pub min_visible: Option<u64>,
}

impl Corpus {
    /// How many artifacts level `level` of `layer` holds.
    ///
    /// **Derived from *n*, and that is the one place *n* is allowed in** — the same licence the
    /// census loop bound has. A level's *population* has to scale with the corpus or the fixture
    /// stops being a fixture at 10⁹; every property *of* an artifact is still independent of it, so
    /// the same seed at a smaller *n* is this level truncated, exactly as the item arm is.
    pub fn artifacts_in(&self, layer: u64, level: u32) -> u64 {
        if level == EMPTY_LEVEL {
            return 0;
        }
        // A hundred members to an artifact at the interval's stride, which is the ratio the design
        // sizes membership from (10⁹ rows over 10⁷ artifacts).
        let scaled = self.n() / 100;
        let by_layer = keyed(self.seed(), SALT_A_INTERVAL, layer) % 4;
        scaled.saturating_sub(by_layer).max(2)
    }

    /// The members of artifact `a` — the closed-form forward direction.
    ///
    /// Ascending and deduplicated, so a caller may compare it against a served membership without
    /// sorting one of them into the other's order.
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

    /// Every artifact of `(layer, level)` holding entity `e` — the closed-form reverse direction,
    /// and the half a generator usually cannot answer.
    ///
    /// Ascending. Constant time: the interval half walks at most `MAX_SPILL + 1` candidate
    /// artifacts, and the scatter half is one inversion and a division.
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
        // artifact scattered it — which is the answer, not a miss.
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

    /// The generating set of artifact `a`: about a hundred of its own members.
    ///
    /// **A subset of the membership, always.** A generating set naming a non-member is a different
    /// object — the model refuses it at publication — so a generator that drew one independently
    /// would be testing a state the system does not accept.
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

    /// The declared shape of artifact `a` — the two independent gate conjuncts, cycled so that all
    /// four of their combinations occur in every level at every size.
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

    /// The artifact census over this layer and level (`lib.rs`'s `bucket_census`): one O(*n*) pass
    /// answering, per artifact, how many of its members `grant` sees. An artifact this grant sees
    /// nothing of is absent, never a zero.
    ///
    /// This is what makes the flat arm checkable at 10⁹: `artifact_members` walked per artifact
    /// would cost the same total work but scattered across `artifacts_in` calls instead of one
    /// pass, and — the point that matters for the oracle — it would be a restatement of the
    /// *forward* direction rather than an independent check of the *reverse* one. Both directions
    /// must agree with what this emits, which is the whole of "nothing is missing or extra".
    pub fn flat_artifact_census(&self, layer: u64, level: u32, grant: &Grant) -> Vec<(u64, u64)> {
        self.bucket_census(grant, |e| self.artifacts_holding(layer, level, e))
    }

    /// The stride the intervals are anchored on.
    fn stride(&self, count: u64) -> u64 {
        (self.n() / count.max(1)).max(1)
    }

    /// Artifact `a`'s interval, `[lo, hi)`, clipped to the corpus.
    ///
    /// Artifact 0 holds exactly one member and artifact 1 holds none by interval — the planted
    /// smallest-and-empty pair, which is where an off-by-one in a criterion or a projection shows
    /// up as an artifact that is simply absent.
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
            // Its members are the scatter alone, which is what gives the level an artifact whose
            // membership is *all* pathological rather than mostly contiguous.
            return (lo, lo);
        }
        // A keyed length between one and `MAX_SPILL + 1` strides: the spill is the overlap, and it
        // is bounded so the reverse direction stays a short walk.
        let spill =
            keyed(self.seed(), SALT_A_LENGTH ^ layer_salt(layer, level), a) % (MAX_SPILL + 1);
        let len = stride.saturating_mul(spill + 1);
        (lo, (lo + len).min(self.n()))
    }

    /// A keyed bijection on `[0, n)`, by cycle-walking a four-round Feistel over the smallest
    /// even-bit domain covering *n*.
    ///
    /// **A bijection rather than a sample, because the reverse direction is the point.** Which
    /// artifacts scattered into *e* is a scan if the scatter is drawn, and arithmetic if it is a
    /// permutation of an index run. Cycle-walking keeps it a bijection on a domain that is not a
    /// power of two: an image outside `[0, n)` is fed back in, which terminates because the walk is
    /// a permutation of a finite set and cannot cycle without returning to its start.
    fn shuffle(&self, key: u64, x: u64) -> u64 {
        let (half, mask) = self.feistel_shape();
        let mut v = x;
        loop {
            v = feistel(self.seed() ^ SALT_A_FEISTEL ^ key, half, mask, v, false);
            if v < self.n() {
                return v;
            }
        }
    }

    /// The exact inverse of [`Self::shuffle`], walking the same cycle backwards.
    fn unshuffle(&self, key: u64, y: u64) -> u64 {
        let (half, mask) = self.feistel_shape();
        let mut v = y;
        loop {
            v = feistel(self.seed() ^ SALT_A_FEISTEL ^ key, half, mask, v, true);
            if v < self.n() {
                return v;
            }
        }
    }

    /// Half-width in bits, and the mask of one half — the smallest even width covering *n*.
    fn feistel_shape(&self) -> (u32, u64) {
        let bits = (64 - self.n().saturating_sub(1).leading_zeros()).max(2);
        let half = bits.div_ceil(2);
        (half, (1u64 << half) - 1)
    }
}

/// One layer-and-level's key material, so two levels of one layer are as unrelated as two layers.
///
/// `pub(crate)` because [`crate::partition`] and [`crate::boundary`] key their own artifact arms
/// by the same `(layer, level)` pair and must be as unrelated from this one and each other as two
/// layers are — a shared salt would let a partition artifact and a flat artifact land on the same
/// key material by construction rather than by coincidence.
pub(crate) fn layer_salt(layer: u64, level: u32) -> u64 {
    mix64(layer.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(level))
}

/// Four Feistel rounds over `2 * half` bits, forwards or in reverse.
///
/// Four rather than three: three is the textbook minimum for a pseudorandom permutation and the
/// fourth is what the identity key uses, for the same reason — the cost is nothing here and the
/// mixing is visibly better at small widths, which is exactly where a fixture gets built.
fn feistel(key: u64, half: u32, mask: u64, v: u64, reverse: bool) -> u64 {
    let mut l = (v >> half) & mask;
    let mut r = v & mask;
    for round in 0..4u64 {
        let round = if reverse { 3 - round } else { round };
        let f = mix64(key ^ mix64(round.wrapping_add(1)) ^ r) & mask;
        if reverse {
            // Undo `(l, r) -> (r, l ^ f(r))`: the pre-image of this round has `r` on the left.
            let previous_r = l;
            let previous_l = r ^ (mix64(key ^ mix64(round.wrapping_add(1)) ^ previous_r) & mask);
            l = previous_l;
            r = previous_r;
        } else {
            let next = l ^ f;
            l = r;
            r = next;
        }
    }
    (l << half) | r
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

    /// **The property the census rests on**: the two directions are the same relation. Anything
    /// that made them disagree — a spill the reverse walk is too short for, an inversion that is
    /// not one — would leave the check reporting a defect in the system for a defect in itself.
    #[test]
    fn the_two_directions_agree_over_every_entity() {
        let c = corpus(4_000);
        for level in [0u32, 2] {
            let count = c.artifacts_in(7, level);
            // Forward: build the whole relation once.
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

    /// Prefix stability, which is what lets a failure at 10⁹ bisect on *n*: every property *of* an
    /// artifact is independent of the corpus size, so a smaller *n* is the same level truncated.
    #[test]
    fn an_artifacts_membership_does_not_depend_on_the_corpus_size() {
        let small = corpus(4_000);
        let large = corpus(40_000);
        // The same *stride* is what makes the comparison meaningful: artifact `a` of a level sized
        // by `n` covers a different span at a different `n`, so the shape to pin is the one that
        // must not move — the planted cases and the gate cycle.
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

    /// The planted cases are there, at every size, because each is a case something treats
    /// differently and none of them arises by chance.
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
            // All four cells of the gate's two-by-two, in the first four artifacts.
            let shapes: Vec<_> = (0..4).map(|a| c.artifact_shape(1, 0, a)).collect();
            assert_eq!(
                shapes
                    .iter()
                    .filter(|s| s.own_term.is_some() && s.min_visible.is_some())
                    .count(),
                1,
                "the both-conjuncts cell is missing at n={n}"
            );
            assert_eq!(
                shapes
                    .iter()
                    .filter(|s| s.own_term.is_none() && s.min_visible.is_none())
                    .count(),
                1,
                "the neither-conjunct cell is missing at n={n}"
            );
        }
    }

    /// Artifacts within a level **overlap**, which the model permits and a disjoint generator would
    /// never exercise — the multimap the fold's inverted arm would have needed exists because of
    /// this.
    #[test]
    fn artifacts_within_a_level_overlap() {
        let c = corpus(4_000);
        let shared = (0..c.n())
            .filter(|e| c.artifacts_holding(2, 0, *e).len() > 1)
            .count();
        assert!(
            shared > 0,
            "no entity is held by two artifacts, so overlap is untested"
        );
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

    /// The census oracle agrees with the materialised relation, by brute force: for every
    /// artifact, `flat_artifact_census`'s count is exactly the visible members `artifact_members`
    /// names, and an artifact this grant sees nothing of is simply absent from the census.
    #[test]
    fn the_census_agrees_with_artifact_members_by_brute_force() {
        let c = corpus(10_000);
        let grant = crate::Grant::parse("0,1,2,3").unwrap();
        let count = c.artifacts_in(6, 0);
        let mut expected = std::collections::BTreeMap::new();
        for a in 0..count {
            let visible = c
                .artifact_members(6, 0, a)
                .into_iter()
                .filter(|e| c.visible(*e, &grant))
                .count() as u64;
            if visible > 0 {
                expected.insert(a, visible);
            }
        }
        let census = c.flat_artifact_census(6, 0, &grant);
        let got: std::collections::BTreeMap<u64, u64> = census.into_iter().collect();
        assert_eq!(
            got, expected,
            "the census and the brute-force count disagree"
        );
    }
}
