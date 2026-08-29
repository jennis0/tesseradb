//! **Which form a level is served in** — the automatic pick, the pin that overrides it, and the
//! observations both are recorded beside ([decision 0094](../../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)).
//!
//! Neither input the pick reads is in the declaration. **How far a membership is spread across row
//! space** is a property of where the data actually landed, and **the artifact count** moves as the
//! corpus grows, as artifacts are published and as a fold retires them. So the layout cannot be a
//! one-off decision taken from a config file, and it cannot be left to the author either: the
//! crossover is a measurement, and it is not something someone writing a layer block should have to
//! hold.
//!
//! # The rule itself lives one crate down
//!
//! [`choose`] and the shape it reads are [`tessera_store::membership`]'s, beside the durable forms
//! they pick between, because **the build chooses too**. A pick that only the fold could reach is
//! what left every freshly built bundle serving its enumerated layers in the wrong form until its
//! first fold — the 10–11× the 2026-08-22 campaign measured. This module is the engine's name for
//! it, so every call site here reads as it did.
//!
//! # What the pick is conservative about
//!
//! The direction of the mistake is asymmetric. Picking artifact-major where row-major would have
//! been cheaper costs latency on a route that is measured, built and correct at every size the
//! campaign reached (`design/artifact-serving-at-scale.md` §5: *there is no serving-speed wall on
//! the corrected structures*). Picking row-major where the level does not suit it costs a whole-map
//! scan of `viewport ∩ M_auth` — 319 ms against 20.1 on the measured partition arm — on exactly the
//! request an artifact-major level answers without scanning anything. So the threshold sits well
//! above the band where artifact-major was measured *improving*, rather than inside it — the
//! argument is at [`ROW_MAJOR_EVERYWHERE_FRACTION`].

pub use tessera_store::derived::{
    choose, LevelShape, ROW_MAJOR_EVERYWHERE_FRACTION, ROW_MAJOR_MIN_ARTIFACTS,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_types::layer::{
        ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration,
        MembershipSource, ServingLayout, ShapeDeclaration, ShapeKind,
    };

    fn shape(artifacts: u64, everywhere: f64, partitions: bool) -> LevelShape {
        LevelShape {
            artifacts,
            // Held at a scattered level's own figure throughout: the pick no longer reads it, and
            // these cases say so by moving the other axis under it.
            blocks_per_artifact: 96.8,
            everywhere_fraction: everywhere,
            partitions,
        }
    }

    fn declaration(membership: MembershipSource, pin: Option<ServingLayout>) -> LayerDeclaration {
        LayerDeclaration {
            name: "clusters/x".into(),
            title: None,
            views: vec!["s0".into()],
            membership,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: ArtifactVisibility::inherited(),
            require_member_visibility: None,
            hierarchy: Hierarchy {
                kind: HierarchyKind::Flat,
                prune_children: false,
            },
            content: ContentDeclaration::default(),
            depends_on: Vec::new(),
            levels: Vec::new(),
            layout: pin,
            shape: None,
        }
    }

    /// **The pick is conservative in the direction of what is built today**, and the band the
    /// bracket measured artifact-major *improving* through — 1.6% to 3.6% of a level too wide for
    /// any node — stays artifact-major at every population, as does everything under the
    /// threshold.
    #[test]
    fn a_level_the_walk_can_place_stays_artifact_major() {
        let enumerated = declaration(MembershipSource::Enumerated, None);
        for everywhere in [0.0, 0.016, 0.023, 0.029, 0.036, 0.1, 0.249] {
            for artifacts in [10, 1_000, 10_000_000] {
                assert_eq!(
                    choose(&enumerated, shape(artifacts, everywhere, true)),
                    ServingLayout::ArtifactMajor,
                    "{everywhere} everywhere over {artifacts} artifacts"
                );
            }
        }
    }

    /// A level the node walk cannot place at a population worth the column flips, and the
    /// label/list split follows from whether the memberships are disjoint rather than from any
    /// number.
    #[test]
    fn a_spread_level_flips_and_the_split_follows_the_membership() {
        let enumerated = declaration(MembershipSource::Enumerated, None);
        assert_eq!(
            choose(&enumerated, shape(10_000, 1.0, true)),
            ServingLayout::RowMajorLabel
        );
        assert_eq!(
            choose(&enumerated, shape(10_000, 1.0, false)),
            ServingLayout::RowMajorList
        );
        // And exactly at the threshold, which is where a `>=` and a `>` differ.
        assert_eq!(
            choose(
                &enumerated,
                shape(10_000, ROW_MAJOR_EVERYWHERE_FRACTION, true)
            ),
            ServingLayout::RowMajorLabel
        );
        // The count tiebreak: the same spread under a thousand artifacts buys nothing measurable.
        assert_eq!(
            choose(&enumerated, shape(999, 1.0, true)),
            ServingLayout::ArtifactMajor
        );
    }

    /// **Blocks per artifact no longer decides anything.** The bracket moved it 6 → 12 with the
    /// cost *falling*, so a level held at one spread answers the same whatever its container count
    /// — which is the whole content of the 2026-08-23 change of axis.
    #[test]
    fn blocks_per_artifact_is_reported_and_does_not_decide() {
        let enumerated = declaration(MembershipSource::Enumerated, None);
        for blocks in [1.0, 6.0, 10.0, 12.0, 108.0, 178.9] {
            let mut clumped = shape(10_000, 0.0, true);
            clumped.blocks_per_artifact = blocks;
            assert_eq!(
                choose(&enumerated, clumped),
                ServingLayout::ArtifactMajor,
                "{blocks} blocks per artifact over a level the walk places"
            );
            let mut spread = shape(10_000, 1.0, true);
            spread.blocks_per_artifact = blocks;
            assert_eq!(
                choose(&enumerated, spread),
                ServingLayout::RowMajorLabel,
                "{blocks} blocks per artifact over a level the walk cannot place"
            );
        }
    }

    /// **A pin never flips**, whatever the observations say — and it reaches a level the automatic
    /// rule would have left alone in either direction.
    #[test]
    fn a_pin_is_read_and_never_re_derived() {
        for observed in [shape(1, 0.0, true), shape(10_000_000, 1.0, false)] {
            for pinned in [
                ServingLayout::RowMajorLabel,
                ServingLayout::ArtifactMajor,
                ServingLayout::RowMajorList,
            ] {
                assert_eq!(
                    choose(
                        &declaration(MembershipSource::Enumerated, Some(pinned)),
                        observed
                    ),
                    pinned
                );
            }
        }
    }

    /// **An attribute level has exactly one layout**, whatever anyone declares and whatever the
    /// observations say — and a pin that says otherwise never validated. A spatial level is picked
    /// as an enumerated one is, and a pin on it holds.
    #[test]
    fn an_attribute_level_has_one_layout_and_a_spatial_level_is_picked() {
        let mut spatial = declaration(MembershipSource::Spatial, None);
        spatial.shape = Some(ShapeDeclaration {
            kind: ShapeKind::Bbox,
        });
        let attribute = declaration(MembershipSource::Attribute("severity".into()), None);
        assert_eq!(
            choose(&spatial, shape(1, 0.0, true)),
            ServingLayout::ArtifactMajor
        );
        assert_eq!(
            choose(&spatial, shape(10_000_000, 1.0, true)),
            ServingLayout::RowMajorLabel
        );
        for observed in [shape(1, 0.0, true), shape(10_000_000, 1.0, false)] {
            assert_eq!(choose(&attribute, observed), ServingLayout::RowMajorLabel);
            for pin in [
                ServingLayout::ArtifactMajor,
                ServingLayout::RowMajorLabel,
                ServingLayout::RowMajorList,
            ] {
                let mut pinned = spatial.clone();
                pinned.layout = Some(pin);
                assert_eq!(choose(&pinned, observed), pin);
                let mut pinned = attribute.clone();
                pinned.layout = Some(pin);
                assert_eq!(choose(&pinned, observed), ServingLayout::RowMajorLabel);
            }
        }
    }

    /// ⊘ A spatial layer that declares no shape holds no artifacts, so it takes the ordinary pick
    /// over an empty level rather than claiming a form it has nothing to serve in.
    #[test]
    fn a_spatial_level_with_no_shape_is_artifact_major() {
        let spatial = declaration(MembershipSource::Spatial, None);
        assert_eq!(
            choose(&spatial, LevelShape::empty()),
            ServingLayout::ArtifactMajor
        );
    }

    /// A level with nothing in it is artifact-major, which is what a registration records: there is
    /// no spread to observe before an artifact lands.
    #[test]
    fn an_empty_level_is_artifact_major() {
        assert_eq!(
            choose(
                &declaration(MembershipSource::Enumerated, None),
                LevelShape::empty()
            ),
            ServingLayout::ArtifactMajor
        );
    }
}
