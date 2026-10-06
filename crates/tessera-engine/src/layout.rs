//! **Which form a level is served in** — the automatic pick, the pin that overrides it, and the
//! observations both are recorded beside.
//!
//! A level with stored memberships whose layer is answered tile by tile (`flat`, `stacked`,
//! `tiered`) is served from a column: its member bitmaps and coverings propose each tile's
//! candidates. Any other level is served from a column only where it is populous and too scattered
//! for the tile index to place, which is a property of where the data landed and moves as the
//! corpus grows, so a fold re-evaluates it.
//!
//! [`choose`] and the shape it reads are [`tessera_store::derived`]'s, beside the durable forms
//! they pick between, because the build chooses too. This module is the engine's name for them.

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
        of_kind(HierarchyKind::Flat, membership, pin)
    }

    fn of_kind(
        kind: HierarchyKind,
        membership: MembershipSource,
        pin: Option<ServingLayout>,
    ) -> LayerDeclaration {
        LayerDeclaration {
            scope: Default::default(),
            name: "clusters/x".into(),
            title: None,
            views: vec!["s0".into()],
            membership,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: ArtifactVisibility::inherited(),
            require_member_visibility: None,
            hierarchy: Hierarchy {
                kind,
                prune_children: false,
            },
            content: ContentDeclaration::default(),
            depends_on: Vec::new(),
            levels: Vec::new(),
            layout: pin,
            shape: None,
        }
    }

    /// **A stored membership answered tile by tile is served from a column at every shape**: the
    /// label form where the memberships are disjoint, the list form where they overlap, whatever
    /// the spread, the container count or the population, including an empty level.
    #[test]
    fn a_tiled_level_with_stored_memberships_is_served_from_a_column() {
        for kind in [
            HierarchyKind::Flat,
            HierarchyKind::Stacked,
            HierarchyKind::Tiered,
        ] {
            let enumerated = of_kind(kind, MembershipSource::Enumerated, None);
            for everywhere in [0.0, 0.016, 0.164, 0.249, 1.0] {
                for artifacts in [10, 1_000, 10_000_000] {
                    assert_eq!(
                        choose(&enumerated, shape(artifacts, everywhere, true)),
                        ServingLayout::RowMajorLabel,
                        "{kind:?}: {everywhere} everywhere over {artifacts} artifacts"
                    );
                    assert_eq!(
                        choose(&enumerated, shape(artifacts, everywhere, false)),
                        ServingLayout::RowMajorList,
                        "{kind:?}: {everywhere} everywhere over {artifacts} overlapping artifacts"
                    );
                }
            }
            assert_eq!(
                choose(&enumerated, LevelShape::empty()),
                ServingLayout::RowMajorLabel
            );
        }
    }

    /// **A treed level is served from a column only where the tile index cannot place it**: at
    /// a quarter of its artifacts too wide for any node and a thousand artifacts, and not below
    /// either. The label/list split follows the membership.
    #[test]
    fn a_treed_level_flips_only_where_it_is_scattered_and_populous() {
        for kind in [HierarchyKind::Nested, HierarchyKind::Dag] {
            let treed = of_kind(kind, MembershipSource::Enumerated, None);
            for everywhere in [0.0, 0.016, 0.1, 0.249] {
                assert_eq!(
                    choose(&treed, shape(10_000_000, everywhere, true)),
                    ServingLayout::ArtifactMajor,
                    "{kind:?}: {everywhere} everywhere"
                );
            }
            assert_eq!(
                choose(&treed, shape(10_000, ROW_MAJOR_EVERYWHERE_FRACTION, true)),
                ServingLayout::RowMajorLabel
            );
            assert_eq!(
                choose(&treed, shape(10_000, 1.0, false)),
                ServingLayout::RowMajorList
            );
            assert_eq!(
                choose(&treed, shape(999, 1.0, true)),
                ServingLayout::ArtifactMajor
            );
            assert_eq!(
                choose(&treed, LevelShape::empty()),
                ServingLayout::ArtifactMajor
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
}
