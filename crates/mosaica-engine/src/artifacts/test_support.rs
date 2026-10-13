//! The fixtures the artifact tests share.

use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use mosaica_lifecycle::membership::Attachment;
use mosaica_lifecycle::Overlay;
use mosaica_types::layer::{
    ArtifactVisibility, ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind,
    LayerDeclaration, MembershipSource, ServingLayout,
};


use crate::tile_index::TileIndex;

use super::*;

/// The declaration of `clusters/a`, drawn on view `s0`, an enumerated flat layer supplying no
/// content.
pub(crate) fn declaration(
    carries_own_labels: bool,
    criterion: Option<ExistenceCriterion>,
) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: "clusters/a".into(),
        title: Some("A".into()),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: if carries_own_labels {
            ArtifactVisibility::carried("visibility")
        } else {
            ArtifactVisibility::inherited()
        },
        require_member_visibility: criterion,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// Row-space memberships without a `RowSpace` to project through — these tests are about the
/// predicate, and building a permutation would test the projection instead.
pub(super) fn rows_of(sets: &[&[u32]]) -> ArtifactRows {
    assembled(
        ArtifactRecords {
            attachments: vec![None; sets.len()],
            parents: vec![Vec::new(); sets.len()],
            declared: vec![Vec::new(); sets.len()],
            access: Vec::new(),
            attached_by: Default::default(),
        },
        MembershipRows {
            rows: sets.iter().map(|s| Some(Arc::new(Bitmap::of(s)))).collect(),
            generating: vec![Vec::new(); sets.len()],
            rows_held: true,
        },
    )
}

/// The two halves plus the index derived over them — the shape [`ArtifactRows::build`] produces
/// from a store, assembled by hand for the cases that are about the predicate.
pub(super) fn assembled(records: ArtifactRecords, membership: MembershipRows) -> ArtifactRows {
    let index = TileIndex::build(&membership, 0);
    ArtifactRows {
        records: std::sync::Arc::new(records),
        membership,
        index: std::sync::Arc::new(index),
        partition: None,
        layout: ServingLayout::ArtifactMajor,
        column: None,
        // Every row these fixtures name is a base row, so the partition answers for all of
        // them. A generating set reaching above the base rows is `tests/artifact_containment.rs`,
        // against a row space that has an extent.
        base_rows: u32::MAX,
        covered: Vec::new(),
        inherited: Vec::new(),
        lists_on_overlap: false,
    }
}

/// The prerequisite where the dependency is served — the ordinary state, so that a case about
/// some other conjunct is not silently answered by this one instead.
pub(super) fn dependency_served(_attachment: &Attachment) -> bool {
    true
}

/// One artifact, with ranked contents given as `(generating set, declared size)` — the
/// declared size separate so a test can build the *lossy projection* case, where row space
/// holds fewer members than the entity-space set the caller published.
pub(super) fn rows_with_contents(members: &[u32], contents: &[(&[u32], u64)]) -> ArtifactRows {
    assembled(
        ArtifactRecords {
            attachments: vec![None],
            parents: vec![Vec::new()],
            declared: vec![contents.iter().map(|(_, declared)| *declared).collect()],
            access: Vec::new(),
            attached_by: Default::default(),
        },
        MembershipRows {
            rows: vec![Some(Arc::new(Bitmap::of(members)))],
            generating: vec![contents.iter().map(|(set, _)| Bitmap::of(set)).collect()],
            rows_held: true,
        },
    )
}

pub(super) struct Fixture {
    pub(super) overlay: Overlay,
    /// The viewer's credential descriptors.
    pub(super) held: FxHashSet<Vec<u8>>,
    /// Whether an artifact with no label is admitted.
    pub(super) unlabelled: bool,
    pub(super) rows: ArtifactRows,
    pub(super) mask: Bitmap,
    pub(super) denied: Bitmap,
}

impl Fixture {
    pub(super) fn new(members: &[&[u32]], mask: &[u32]) -> Self {
        Fixture {
            overlay: Overlay::new(),
            held: FxHashSet::default(),
            unlabelled: true,
            rows: rows_of(members),
            mask: Bitmap::of(mask),
            denied: Bitmap::new(),
        }
    }

    pub(super) fn view<'a>(
        &'a self,
        declaration: &'a LayerDeclaration,
        reachable: bool,
    ) -> ArtifactView<'a, Bitmap> {
        ArtifactView {
            declaration,
            overlay: &self.overlay,
            labels: LabelGate::new(&self.held, self.unlabelled),
            layer_reachable: reachable,
            dependency_served: &dependency_served,
            sides: one_side(&self.rows, &self.mask, &self.denied),
        }
    }
}

impl Fixture {
    /// Give the artifact at `ordinal` its own access label.
    pub(super) fn label(&mut self, ordinal: u32, access: &[&[u8]]) {
        let records = Arc::make_mut(&mut self.rows.records);
        let idx = ordinal as usize;
        if records.access.len() <= idx {
            records.access.resize_with(idx + 1, || None);
        }
        let access: Vec<Vec<u8>> = access.iter().map(|d| d.to_vec()).collect();
        records.access[idx] = Some(super::rows::parse_access(&access));
    }
}

/// The sides of a view of one shard: `rows`, seen through `mask`, with `denied` denied and no
/// containment partition or masked counts.
pub(super) fn one_side<'a>(
    rows: &'a ArtifactRows,
    mask: &'a Bitmap,
    denied: &'a Bitmap,
) -> Vec<ArtifactSide<'a, Bitmap>> {
    vec![ArtifactSide {
        rows,
        mask: Some(mask),
        denied,
        containment: None,
        counts: None,
    }]
}

/// What the one predicate serves of `rows`' artifact at `ordinal`, as a containment answer, to a
/// viewer seeing `mask`, with `denied` denied and the partition's `answers` where given, on a layer
/// declaring supplied content.
pub(super) fn contained(
    rows: &ArtifactRows,
    ordinal: u32,
    mask: &Bitmap,
    denied: &Bitmap,
    answers: Option<crate::containment::ContainmentAnswers<'_>>,
) -> Containment {
    let declaration = LayerDeclaration {
        content: ContentDeclaration {
            computed: Vec::new(),
            supplied: vec![mosaica_types::layer::SuppliedContent {
                name: "label".into(),
                ty: "text".into(),
                require_member_visibility: mosaica_types::layer::SuppliedRequirement::All,
            }],
        },
        ..declaration(false, None)
    };
    let view = ArtifactView {
        declaration: &declaration,
        overlay: &Overlay::new(),
        labels: open_labels(),
        layer_reachable: true,
        dependency_served: &dependency_served,
        sides: vec![ArtifactSide {
            rows,
            mask: Some(mask),
            denied,
            containment: answers,
            counts: None,
        }],
    };
    match view.verdict(mosaica_types::EntityId::new(999), ordinal) {
        ArtifactVerdict::Serve { rank: Some(rank), .. } => Containment::Satisfied(rank),
        ArtifactVerdict::Serve { rank: None, .. } => Containment::NothingToContain,
        ArtifactVerdict::Absent(Withheld::Containment) => Containment::Unsatisfied,
        other => panic!("withheld before containment: {other:?}"),
    }
}

/// No row denied.
pub(super) fn no_denies() -> &'static Bitmap {
    static EMPTY: std::sync::OnceLock<Bitmap> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Bitmap::new)
}

/// A label test that admits every artifact with no label and holds no descriptor.
pub(super) fn open_labels() -> LabelGate<'static> {
    static EMPTY: std::sync::OnceLock<FxHashSet<Vec<u8>>> = std::sync::OnceLock::new();
    LabelGate::new(EMPTY.get_or_init(FxHashSet::default), true)
}
