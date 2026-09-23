//! The fixtures the artifact tests share.

use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use tessera_lifecycle::membership::Attachment;
use tessera_lifecycle::Overlay;
use tessera_types::layer::{LayerDeclaration, ServingLayout};


use crate::tile_index::TileIndex;

use super::*;

/// Row-space memberships without a `RowSpace` to project through — these tests are about the
/// predicate, and building a permutation would test the projection instead.
pub(super) fn rows_of(sets: &[&[u32]]) -> ArtifactRows {
    assembled(
        ArtifactRecords {
            attachments: vec![None; sets.len()],
            parents: vec![Vec::new(); sets.len()],
            declared: vec![Vec::new(); sets.len()],
            access: Vec::new(),
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
            rows: &self.rows,
            mask: &self.mask,
            dependency_served: &dependency_served,
            containment: None,
            denied: &self.denied,
            counts: None,
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
        records.access[idx] = Some(Arc::from(access.as_slice()));
    }
}

/// A label test that admits every artifact with no label and holds no descriptor.
pub(super) fn open_labels() -> LabelGate<'static> {
    static EMPTY: std::sync::OnceLock<FxHashSet<Vec<u8>>> = std::sync::OnceLock::new();
    LabelGate::new(EMPTY.get_or_init(FxHashSet::default), true)
}
