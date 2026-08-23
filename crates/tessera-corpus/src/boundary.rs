//! The spatial (boundary) arm of the generated corpus: authored geometry, standing in for
//! `membership = "spatial"` (`annotation-representation.md` §2.0).
//!
//! A spatial predicate is *"the points inside this shape"* — no membership storage at all, decoded
//! into Morton ranges at request time, and never stale. The design specifies it; the engine does
//! not yet build it (`artifact-delivery.md` §5.1's "declared and unbuilt"), so what this arm owes
//! the campaign is a fixture the future predicate can be checked against, not a workaround for the
//! missing machinery.
//!
//! **The shape is a depth-`d` Morton tile, one per artifact, kept as simple as the model allows.**
//! Every prefix in `[0, 4^d)` at that depth is a candidate; a keyed rule marks roughly one in
//! [`SELECT_DENSITY`] of them "authored", and the tile's own prefix *is* the artifact's key — there
//! is no separate id to keep in step with it. Both directions follow from that one rule:
//!
//! - **Reverse (which artifact holds entity *e*)** is O(1): quantise *e*'s position to a depth-`d`
//!   tile exactly as [`Corpus::census`] does, then test whether that one prefix was selected.
//! - **Forward (an artifact's members)** has no `O(1)` shortcut in *this* corpus, because entities
//!   are not stored in Morton order — position is a keyed function of `e`, uncorrelated with `e`'s
//!   own order, unlike a build's Morton-sorted rows where a tile is a contiguous range. It is an
//!   O(*n*) scan, the same order [`Corpus::census`] already spends, and the artifact **roster**
//!   never pays it: enumerating which of the `4^d` prefixes are authored is a walk over the tile
//!   space alone, bounded by [`MAX_DEPTH`], independent of *n*.
//!
//! Because the rule reads only a tile prefix and the two salts that key it, nothing here depends on
//! `n` at all — the strongest form of prefix stability any arm in this crate has.

use tessera_spatial::{cell, interleave_bits};

use crate::artifacts::layer_salt;
use crate::{keyed, Corpus, Grant};

const SALT_B_DEPTH: u64 = crate::salt(b"bnd-dep ");
const SALT_B_SELECT: u64 = crate::salt(b"bnd-sel ");

/// The shallowest depth a boundary layer draws — deep enough that a tile is a small fraction of
/// the grid rather than one of a handful of quadrants.
const MIN_DEPTH: u8 = 2;
/// The deepest depth a boundary layer draws. `4^6 = 4096` tiles, which keeps
/// [`Corpus::boundary_artifacts`]'s roster walk (`O(4^d)`, independent of `n`) cheap at every
/// corpus size — the roster is a property of the layer's geometry, not of how many points there
/// are.
const MAX_DEPTH: u8 = 6;
/// Roughly one tile in this many is authored — sparse enough that a boundary layer is a real
/// minority of the grid, the shape a background/foreground boundary actually has.
const SELECT_DENSITY: u64 = 7;

impl Corpus {
    /// The Morton depth this layer's boundaries are drawn at — fixed per `(layer, level)`, never
    /// per artifact: every boundary artifact of one level is a tile of the *same* depth, which is
    /// what lets the reverse direction test one prefix rather than search across depths.
    pub fn boundary_depth(&self, layer: u64, level: u32) -> u8 {
        let span = u64::from(MAX_DEPTH - MIN_DEPTH + 1);
        MIN_DEPTH
            + (keyed(self.seed(), SALT_B_DEPTH ^ layer_salt(layer, level), layer) % span) as u8
    }

    /// Whether depth-`d` tile `prefix` is one of this layer's authored boundaries — the one rule
    /// both directions are built from.
    pub fn boundary_is_authored(&self, layer: u64, level: u32, prefix: u64) -> bool {
        keyed(
            self.seed(),
            SALT_B_SELECT ^ layer_salt(layer, level),
            prefix,
        )
        .is_multiple_of(SELECT_DENSITY)
    }

    /// Every authored artifact of `(layer, level)`, ascending by prefix — the roster a build's
    /// artifact source names. `O(4^depth)`, not `O(n)`: this is geometry, not membership.
    pub fn boundary_artifacts(&self, layer: u64, level: u32) -> Vec<u64> {
        let depth = self.boundary_depth(layer, level);
        let total = 1u64 << (2 * u32::from(depth));
        (0..total)
            .filter(|&prefix| self.boundary_is_authored(layer, level, prefix))
            .collect()
    }

    /// The one boundary artifact holding entity `e`, or `None` where `e` sits outside every
    /// authored tile — the ordinary case, since [`SELECT_DENSITY`] leaves most tiles unauthored.
    ///
    /// O(1): quantises `e`'s position to depth `d` exactly as [`Corpus::census`] does (the same
    /// `cell`/`interleave_bits` pair, so a build's own tile assignment and this one cannot
    /// disagree about which cell a position lands in), then tests that one prefix.
    pub fn boundary_artifact_of(&self, layer: u64, level: u32, e: u64) -> Option<u64> {
        if e >= self.n() {
            return None;
        }
        let item = self.item(e);
        let depth = self.boundary_depth(layer, level);
        let shift = 16 - u32::from(depth);
        let cx = cell(f64::from(item.x), self.extent().x_min, self.extent().x_max);
        let cy = cell(f64::from(item.y), self.extent().y_min, self.extent().y_max);
        let prefix = interleave_bits(u32::from(cx) >> shift, u32::from(cy) >> shift, depth);
        self.boundary_is_authored(layer, level, prefix)
            .then_some(prefix)
    }

    /// The members of authored artifact `prefix` — the forward direction, an O(*n*) scan (module
    /// doc explains why no shorter form exists here). Used by the small brute-force tests and by a
    /// caller materialising an explicit membership list at a size where that is affordable; the
    /// census (below) does not call this per artifact, gathering the same answer in one pass over
    /// entity space instead.
    pub fn boundary_members(&self, layer: u64, level: u32, prefix: u64) -> Vec<u64> {
        (0..self.n())
            .filter(|&e| self.boundary_artifact_of(layer, level, e) == Some(prefix))
            .collect()
    }

    /// The boundary census: one O(*n*) pass, per-artifact visible counts — see
    /// [`crate::Corpus::bucket_census`].
    pub fn boundary_artifact_census(
        &self,
        layer: u64,
        level: u32,
        grant: &Grant,
    ) -> Vec<(u64, u64)> {
        self.bucket_census(grant, |e| {
            self.boundary_artifact_of(layer, level, e)
                .into_iter()
                .collect()
        })
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

    /// At least one tile is authored and at least one is not, at every size — otherwise the
    /// "sparse minority" shape this arm exists to exercise is untested.
    #[test]
    fn some_tiles_are_authored_and_some_are_not() {
        let c = corpus(10_000);
        let roster = c.boundary_artifacts(11, 0);
        let depth = c.boundary_depth(11, 0);
        let total = 1u64 << (2 * u32::from(depth));
        assert!(!roster.is_empty(), "no tile is authored at all");
        assert!(
            (roster.len() as u64) < total,
            "every tile is authored, so the unauthored case is untested"
        );
    }

    /// The two directions are the same relation: every entity in an authored tile's brute-force
    /// membership resolves back to it, and an entity in no authored tile resolves to `None`.
    #[test]
    fn the_two_directions_agree_over_every_entity() {
        let c = corpus(4_000);
        let roster = c.boundary_artifacts(3, 0);
        let mut expected: std::collections::HashMap<u64, Option<u64>> =
            std::collections::HashMap::new();
        for &prefix in &roster {
            for e in c.boundary_members(3, 0, prefix) {
                assert_eq!(
                    expected.insert(e, Some(prefix)),
                    None,
                    "entity {e} sits in two authored tiles"
                );
            }
        }
        for e in 0..c.n() {
            assert_eq!(
                c.boundary_artifact_of(3, 0, e),
                expected.get(&e).copied().unwrap_or(None),
                "the reverse direction disagrees at entity {e}"
            );
        }
    }

    /// Nothing here depends on `n` at all — the strongest prefix-stability claim any arm makes,
    /// because a tile is a property of position and the key material alone.
    #[test]
    fn the_boundary_rule_does_not_depend_on_the_corpus_size() {
        let small = corpus(1_000);
        let large = corpus(1_000_000);
        for layer in 0..6 {
            assert_eq!(
                small.boundary_depth(layer, 0),
                large.boundary_depth(layer, 0)
            );
            assert_eq!(
                small.boundary_artifacts(layer, 0),
                large.boundary_artifacts(layer, 0)
            );
        }
    }

    /// The census oracle agrees with the materialised relation, by brute force.
    #[test]
    fn the_census_agrees_with_boundary_members_by_brute_force() {
        let c = corpus(10_000);
        let grant = crate::Grant::parse("10,11,12,13").unwrap();
        let roster = c.boundary_artifacts(4, 0);
        let mut expected = std::collections::BTreeMap::new();
        for &prefix in &roster {
            let visible = c
                .boundary_members(4, 0, prefix)
                .into_iter()
                .filter(|e| c.visible(*e, &grant))
                .count() as u64;
            if visible > 0 {
                expected.insert(prefix, visible);
            }
        }
        let census = c.boundary_artifact_census(4, 0, &grant);
        let got: std::collections::BTreeMap<u64, u64> = census.into_iter().collect();
        assert_eq!(
            got, expected,
            "the census and the brute-force count disagree"
        );
    }
}
