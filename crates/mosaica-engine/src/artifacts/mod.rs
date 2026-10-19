//! The row-space forms an annotation layer's memberships take, the per-deployment cache that holds
//! and maintains them, and the per-principal verdict computed over one.
//!
//! `rows*.rs` defines [`ArtifactRows`], one level's membership as row-space bitmaps plus the small
//! per-ordinal facts (attachment, parent, declared size) a verdict needs. `projections*.rs` defines
//! [`ArtifactProjections`], the cache of these forms keyed by shard, view and level, and how a
//! growth, a flush, a merge or a publication brings a held form forward rather than rebuilding it.
//! Each point shard's form holds that shard's slice of every membership and generating set, and a
//! slot for every ordinal of the level.
//! `sources.rs` is where a predicate level's membership is resolved from when it has no stored one.
//! `slice.rs` is one point shard's slice of a level, as a form of it walks the level.
//! `view.rs` defines [`ArtifactView`] and its `verdict`: the one predicate for whether an artifact
//! is served to a principal, and the masked count beside it.
//!
//! The cache carries no authorisation: a held [`ArtifactRows`] is a layer's membership, unmasked,
//! the same for every viewer. Everything a viewer is told — a count, a candidacy test, an extent —
//! is answered by [`ArtifactView`] from a form through that viewer's composed mask
//! ([`crate::compose::MaskedSet`]), never from the form alone; reading the form directly would
//! disclose which entities carry an artifact regardless of who may see them.


mod projections;
mod projections_build;
mod projections_forward;
mod rows;
mod rows_build;
mod rows_forward;
mod slice;
mod sources;
#[cfg(test)]
pub(crate) mod test_support;
mod view;

pub use projections::{
    serves_column_only, ArtifactProjections, DeltaKind, DeltaRows,
    LevelDelta, SegmentRows, SetPage, ROW_COLUMN_SCRATCH_DIR,
};
pub use rows::{
    ArtifactRecords, ArtifactRows, Candidacy, Containment, Matched, MembershipRows,
};
pub use sources::{AttributeSource, PredicateSource, SpatialSource};
pub(crate) use slice::LevelSlice;
pub use view::{ArtifactSide, ArtifactVerdict, ArtifactView, LabelGate, Withheld};
pub(crate) use view::{verdict, ArtifactFacts, DEPENDENCY_CHAIN_MAX};

pub(crate) use rows::{satisfied_rank_over, view_key};

use projections::{Coordinate, Held, LevelAddress, ProjectionKey};
use rows::{covered_by, drawn_record, total_rows};
