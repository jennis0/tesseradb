//! **Which form a level is served in** — the automatic pick, the pin that overrides it, and the
//! observations both are recorded beside ([decision 0094](../../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)).
//!
//! Neither input the pick reads is in the declaration. **Blocks per artifact** is a property of
//! where the data actually landed, and **the artifact count** moves as the corpus grows, as
//! artifacts are published and as a fold retires them. So the layout cannot be a one-off decision
//! taken from a config file, and it cannot be left to the author either: the crossover is a
//! measurement, and it is not something someone writing a layer block should have to hold.
//!
//! # One function, so the campaign can tune it
//!
//! [`choose`] is the whole of the rule. Its two constants are ⊘ **provisional** and are argued at
//! their definitions rather than here; what matters structurally is that there is one of them,
//! called from the registration and from the fold alike, so a later sweep changes the pick
//! everywhere by changing one number.
//!
//! # What the pick is conservative about
//!
//! The direction of the mistake is asymmetric. Picking artifact-major where row-major would have
//! been cheaper costs latency on a route that is measured, built and correct at every size the
//! campaign reached (`design/artifact-serving-at-scale.md` §5: *there is no serving-speed wall on
//! the corrected structures*). Picking row-major where the level does not suit it costs a whole-map
//! scan of `viewport ∩ M_auth` — 319 ms against 20.1 on the measured partition arm — on exactly the
//! request an artifact-major level answers without scanning anything. So the threshold sits at the
//! **low end of the measured scattered band** rather than in the middle of the unmeasured gap, and
//! every flip it makes is inside territory a measurement covers.

use tessera_types::layer::{LayerDeclaration, MembershipSource, ServingLayout};

/// ⊘ **Provisional: blocks per artifact at or above which a level is served row-major.**
///
/// The axis is measured and separates the two costs by two decades; the point on it is not. Every
/// recorded run sits at **1.0–1.6** blocks per artifact or at **10.0–96.8**, both ends constructed
/// by the fixture's generator rather than observed, and **nothing was measured between 1.6 and 10**
/// (scale memo §5). The `dispersed` arm that was meant to bracket it ran at 2, 4, 6 and 8 and broke
/// monotonicity at 8 — 51 ms whole-map against 61 ms at *two* blocks per artifact — with nothing in
/// the fixture statistics accounting for it, and a targeted re-run is queued.
///
/// So this is **10.0**: the low end of the band where a measurement exists. It is deliberately not
/// the middle of the unmeasured gap, because a flip made there would be a guess in the expensive
/// direction — see the module doc. The campaign's sweep is what moves it.
pub const ROW_MAJOR_BLOCKS_PER_ARTIFACT: f64 = 10.0;

/// ⊘ **Provisional: the artifact count below which the pick stays artifact-major whatever the
/// locality.**
///
/// A thousand. Below it the whole-map cell is milliseconds on either route — the measured cells run
/// in single-digit milliseconds at 10³ artifacts — so a flip buys nothing measurable and costs a
/// column, a manifest entry and a per-session histogram. Above it the row-major scan's `O(visible
/// rows)` starts to be paid against a per-candidate cost that is climbing with the population.
///
/// It is a **tiebreak and not a bound**: a level under it that is *pinned* row-major is served
/// row-major, because a pin is an operator saying they know something the observations do not.
pub const ROW_MAJOR_MIN_ARTIFACTS: u64 = 1_000;

/// What one level looks like, as the fold observes it — **after** the retirements the fold executes,
/// which is exactly the case the re-evaluation exists for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelShape {
    /// How many live artifacts the level holds. Holes are not counted: a retired slot is not an
    /// artifact, and counting it would keep a level that has been emptied looking populous.
    pub artifacts: u64,
    /// The mean number of Roaring containers a membership touches. **The measured cost model is
    /// that bitmap operations cost O(containers touched) rather than O(cardinality)**, so this is
    /// the number that says whether the level has row-space locality — one block per artifact is a
    /// cluster, hundreds is a scattered predicate.
    pub blocks_per_artifact: f64,
    /// Whether the memberships are disjoint — which decides the **label/list** split and is not a
    /// choice. Observed rather than declared: single-valuedness is a property of the data.
    pub partitions: bool,
}

impl LevelShape {
    /// The shape of a level with nothing in it — what a registration sees, since a level with no
    /// artifacts has no locality to observe.
    pub fn empty() -> Self {
        LevelShape {
            artifacts: 0,
            blocks_per_artifact: 0.0,
            partitions: true,
        }
    }
}

/// **The one rule.** `pin` is the layer's declared override, which is read and never re-derived.
///
/// `source` decides representability before anything else: a shape has no per-row source, and
/// inverting its ranges into a column would materialise the membership the ranges exist to avoid.
/// A row-major pin on such a layer is refused at parse
/// (`tessera_types::layer::DeclarationError::LayoutWithoutRowSource`), so reaching here with one is
/// a declaration that never validated — answered artifact-major rather than trusted.
///
/// **A pin never flips**, at the build or at any fold after it. That is the whole point of pinning:
/// a layer whose measured shape says one thing and whose operator knows another — a level about to
/// be grown, a benchmark, a bug being cornered. A nightly fold that silently reverted it would make
/// the key a suggestion.
///
/// **A pinned `column` on a level that does not partition is not corrected here.** Whether the
/// memberships are disjoint is checked where the column is built, and a double claim declines the
/// column and leaves the level artifact-major with a loud trace — so the fallback is one decision at
/// one place rather than a rule this function and the builder would each have to hold.
pub fn choose(declaration: &LayerDeclaration, shape: LevelShape) -> ServingLayout {
    // **A predicate's form follows from its membership and is never re-derived.** A shape's
    // members are row ranges recomputed per request; a single-valued attribute's members *are* the
    // column, one label per row. Neither has a second form to be chosen between, which is why
    // `LayerDeclaration::validate` refuses a pin on either and why the fold's re-evaluation reaches
    // here and leaves both alone.
    //
    // ⊘ A spatial layer that declares no `shape` has no ranges to serve and holds no artifacts, so
    // it falls through to the ordinary pick and lands artifact-major over an empty level.
    match declaration.membership {
        MembershipSource::Spatial if declaration.shape.is_some() => {
            return ServingLayout::SpatialRanges
        }
        MembershipSource::Attribute(_) => return ServingLayout::RowMajorLabel,
        MembershipSource::Spatial | MembershipSource::Enumerated => {}
    }
    if let Some(pinned) = declaration.layout {
        return pinned;
    }
    let row_major = shape.artifacts >= ROW_MAJOR_MIN_ARTIFACTS
        && shape.blocks_per_artifact >= ROW_MAJOR_BLOCKS_PER_ARTIFACT;
    if !row_major {
        return ServingLayout::ArtifactMajor;
    }
    // **The label/list split follows from the membership, never from the numbers.** A level whose
    // memberships are disjoint has exactly one label per row; one whose memberships overlap needs a
    // list, and pays the larger constant for it.
    if shape.partitions {
        ServingLayout::RowMajorLabel
    } else {
        ServingLayout::RowMajorList
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_types::layer::{
        ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, ShapeDeclaration,
        ShapeKind,
    };

    fn shape(artifacts: u64, blocks: f64, partitions: bool) -> LevelShape {
        LevelShape {
            artifacts,
            blocks_per_artifact: blocks,
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

    /// **The pick is conservative in the direction of what is built today.** The clustered and
    /// regional bands the campaign measured — 1.0 and 1.0–1.6 blocks per artifact — stay
    /// artifact-major at every population, and so does the whole unmeasured gap below the
    /// threshold.
    #[test]
    fn locality_keeps_a_clustered_or_regional_level_artifact_major() {
        let enumerated = declaration(MembershipSource::Enumerated, None);
        for blocks in [1.0, 1.6, 2.0, 8.0, 9.9] {
            for artifacts in [10, 1_000, 10_000_000] {
                assert_eq!(
                    choose(&enumerated, shape(artifacts, blocks, true)),
                    ServingLayout::ArtifactMajor,
                    "{blocks} blocks per artifact over {artifacts} artifacts"
                );
            }
        }
    }

    /// A scattered level at a population worth the column flips, and the label/list split follows
    /// from whether the memberships are disjoint rather than from any number.
    #[test]
    fn a_scattered_level_flips_and_the_split_follows_the_membership() {
        let enumerated = declaration(MembershipSource::Enumerated, None);
        assert_eq!(
            choose(&enumerated, shape(10_000, 96.8, true)),
            ServingLayout::RowMajorLabel
        );
        assert_eq!(
            choose(&enumerated, shape(10_000, 96.8, false)),
            ServingLayout::RowMajorList
        );
        // The count tiebreak: the same locality under a thousand artifacts buys nothing measurable.
        assert_eq!(
            choose(&enumerated, shape(999, 96.8, true)),
            ServingLayout::ArtifactMajor
        );
    }

    /// **A pin never flips**, whatever the observations say — and it reaches a level the automatic
    /// rule would have left alone in either direction.
    #[test]
    fn a_pin_is_read_and_never_re_derived() {
        for observed in [shape(1, 1.0, true), shape(10_000_000, 96.8, false)] {
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

    /// **A predicate level has exactly one layout**, whatever anyone declares and whatever the
    /// observations say — and a pin that says otherwise never validated.
    #[test]
    fn a_predicate_level_has_one_layout() {
        let mut spatial = declaration(MembershipSource::Spatial, None);
        spatial.shape = Some(ShapeDeclaration {
            kind: ShapeKind::Bbox,
            depth: 6,
        });
        let attribute = declaration(MembershipSource::Attribute("severity".into()), None);
        for observed in [shape(1, 1.0, true), shape(10_000_000, 96.8, false)] {
            assert_eq!(choose(&spatial, observed), ServingLayout::SpatialRanges);
            assert_eq!(choose(&attribute, observed), ServingLayout::RowMajorLabel);
            for pin in [
                ServingLayout::ArtifactMajor,
                ServingLayout::RowMajorLabel,
                ServingLayout::RowMajorList,
            ] {
                let mut pinned = spatial.clone();
                pinned.layout = Some(pin);
                assert_eq!(choose(&pinned, observed), ServingLayout::SpatialRanges);
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
    /// no locality to observe before an artifact lands.
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
