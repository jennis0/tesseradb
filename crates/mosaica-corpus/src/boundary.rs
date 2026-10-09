//! The spatial (boundary) kind of the generated corpus: authored geometry, standing in for
//! `membership = "spatial"`.
//!
//! A spatial predicate is the points inside a shape: no membership storage, decoded into Morton
//! ranges at request time. Not built yet: the engine does not decode a spatial predicate, so this
//! module is a fixture for that future predicate.
//!
//! The shape is a depth-`d` Morton tile, one per artifact. Every prefix in `[0, 4^d)` at that depth
//! is a candidate; a keyed rule marks roughly one in [`SELECT_DENSITY`] of them authored, and the
//! tile's own prefix is the artifact's key.
//!
//! - Reverse (which artifact holds entity `e`) is O(1): quantise `e`'s position to a depth-`d`
//!   tile, then test whether that one prefix was selected.
//! - Forward (an artifact's members) is an O(n) scan, because entities are not stored in Morton
//!   order here. The artifact roster never pays this: enumerating which of the `4^d` prefixes are
//!   authored is a walk over the tile space alone, bounded by [`MAX_DEPTH`].
//!
//! Nothing here depends on `n`.

use crate::artifacts::layer_salt;
use crate::{keyed, Corpus, Grant};

const SALT_B_DEPTH: u64 = crate::salt(b"bnd-dep ");
const SALT_B_SELECT: u64 = crate::salt(b"bnd-sel ");

/// The shallowest depth a boundary layer draws: deep enough that a tile is a small fraction of
/// the grid.
const MIN_DEPTH: u8 = 2;
/// The deepest depth a boundary layer draws. `4^6 = 4096` tiles, which keeps
/// [`Corpus::boundary_artifacts`]'s roster walk (`O(4^d)`, independent of `n`) cheap.
const MAX_DEPTH: u8 = 6;
/// Roughly one tile in this many is authored.
const SELECT_DENSITY: u64 = 7;

impl Corpus {
    /// The Morton depth this layer's boundaries are drawn at, fixed per `(layer, level)`, never
    /// per artifact.
    pub fn boundary_depth(&self, layer: u64, level: u32) -> u8 {
        let span = u64::from(MAX_DEPTH - MIN_DEPTH + 1);
        MIN_DEPTH
            + (keyed(self.seed(), SALT_B_DEPTH ^ layer_salt(layer, level), layer) % span) as u8
    }

    /// Whether depth-`d` tile `prefix` is one of this layer's authored boundaries.
    pub fn boundary_is_authored(&self, layer: u64, level: u32, prefix: u64) -> bool {
        keyed(
            self.seed(),
            SALT_B_SELECT ^ layer_salt(layer, level),
            prefix,
        )
        .is_multiple_of(SELECT_DENSITY)
    }

    /// Every authored artifact of `(layer, level)`, ascending by prefix. `O(4^depth)`, independent
    /// of `n`.
    pub fn boundary_artifacts(&self, layer: u64, level: u32) -> Vec<u64> {
        let depth = self.boundary_depth(layer, level);
        let total = 1u64 << (2 * u32::from(depth));
        (0..total)
            .filter(|&prefix| self.boundary_is_authored(layer, level, prefix))
            .collect()
    }

    /// The one boundary artifact holding entity `e`, or `None` where `e` sits outside every
    /// authored tile. O(1).
    pub fn boundary_artifact_of(&self, layer: u64, level: u32, e: u64) -> Option<u64> {
        if e >= self.n() {
            return None;
        }
        let prefix = self.tile_of(e, self.boundary_depth(layer, level));
        self.boundary_is_authored(layer, level, prefix)
            .then_some(prefix)
    }

    /// The members of authored artifact `prefix`: the forward direction, an O(n) scan.
    pub fn boundary_members(&self, layer: u64, level: u32, prefix: u64) -> Vec<u64> {
        (0..self.n())
            .filter(|&e| self.boundary_artifact_of(layer, level, e) == Some(prefix))
            .collect()
    }

    /// The boundary census: one O(n) pass, per-artifact visible counts.
    pub fn boundary_artifact_census(
        &self,
        layer: u64,
        level: u32,
        grant: &Grant,
    ) -> Vec<(u64, u64)> {
        self.bucket_census(grant, |e| self.boundary_artifact_of(layer, level, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{assert_census_counts_visible_members, corpus};

    /// At least one tile is authored and at least one is not, at every size.
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

    /// Every entity in an authored tile's brute-force membership resolves back to it, and an
    /// entity in no authored tile resolves to `None`.
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

    /// Nothing here depends on `n` at all: a tile is a property of position and the key material
    /// alone.
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

    #[test]
    fn the_census_counts_each_artifacts_visible_members() {
        let c = corpus(10_000);
        let grant = Grant::parse("10,11,12,13").unwrap();
        assert_census_counts_visible_members(
            &c,
            &grant,
            c.boundary_artifact_census(4, 0, &grant),
            c.boundary_artifacts(4, 0).into_iter(),
            |prefix| c.boundary_members(4, 0, prefix),
        );
    }
}
