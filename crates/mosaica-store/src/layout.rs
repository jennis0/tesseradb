//! Where a prefix's files live: the term dictionary, each partition's side-manifests, and each
//! point shard's per-entity and per-view structures.
//!
//! A path one side writes and another derives again comes from here, so the two cannot disagree.
//! The `_rel` forms are prefix-relative with forward slashes, as the manifests' `files` maps key
//! them; a file on disc is the prefix directory joined with one.

use std::path::{Path, PathBuf};

use mosaica_types::view::{ViewIncarnation, DECLARED_INCARNATION};
use mosaica_types::ShardId;

use crate::edited::Direction;

/// The separator between a group's name and a view's key in a view id. It is reserved out of plain
/// view names and keys, so splitting on it is unambiguous.
pub use mosaica_types::view::GROUP_SEPARATOR;

/// The term dictionary's directory under a prefix, one for the whole bundle.
pub const DICTIONARY_DIR: &str = "dictionary";

/// The dictionary extent a flush or a coalesce writes, prefix-relative, where `id` is the flush's
/// segment id or the coalesce's id.
pub fn dictionary_extent_rel(id: &str) -> String {
    format!("{DICTIONARY_DIR}/{id}/terms-0.dict")
}

/// The directory every partition's directory is in, under `prefix_dir`.
pub fn partitions_dir(prefix_dir: &Path) -> PathBuf {
    prefix_dir.join("partitions")
}

/// A partition's directory, prefix-relative: where its `SEGMENTS-<n>.json` are.
pub fn partition_rel(partition: &str) -> String {
    format!("partitions/{partition}")
}

/// [`partition_rel`] under `prefix_dir`.
pub fn partition_dir(prefix_dir: &Path, partition: &str) -> PathBuf {
    prefix_dir.join(partition_rel(partition))
}

/// The path components a view id names, in order: one for a plain view, two for a group's view,
/// which is nested rather than joined because `:` is not a path character everywhere.
pub fn view_path_components(view_id: &str) -> Vec<&str> {
    match view_id.split_once(GROUP_SEPARATOR) {
        Some((group, key)) => vec![group, key],
        None => vec![view_id],
    }
}

/// One point shard of a partition: every per-entity and per-view structure of the shard is under
/// its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardLayout<'a> {
    pub partition: &'a str,
    pub shard: ShardId,
}

impl<'a> ShardLayout<'a> {
    pub const fn new(partition: &'a str, shard: ShardId) -> Self {
        ShardLayout { partition, shard }
    }

    /// The shard's directory, prefix-relative: `partitions/<partition>/shards/<shard>`.
    pub fn rel(self) -> String {
        format!(
            "{}/shards/{}",
            partition_rel(self.partition),
            self.shard.raw()
        )
    }

    /// The shard's directory under `prefix_dir`.
    pub fn dir(self, prefix_dir: &Path) -> PathBuf {
        prefix_dir.join(self.rel())
    }

    /// `views/<view>` or `views/<group>/<key>`: a view's permutation, row-to-entity table, field
    /// tallies and segments.
    pub fn view_rel(self, view_id: &str) -> String {
        let mut rel = format!("{}/views", self.rel());
        for component in view_path_components(view_id) {
            rel.push('/');
            rel.push_str(component);
        }
        rel
    }

    /// One segment's directory in a view.
    pub fn segment_rel(self, view_id: &str, seg_id: &str) -> String {
        format!("{}/segments/{seg_id}", self.view_rel(view_id))
    }

    /// `attrs/<column>/<view components>`: one view's column of a group-scoped family.
    ///
    /// Above [`DECLARED_INCARNATION`] the last component carries `@<incarnation>`, so a key created
    /// again writes its base beside its predecessor's, which the live manifest may still name and a
    /// live generation may still map. `@` is reserved out of a view key.
    pub fn scoped_column_rel(
        self,
        column: &str,
        view_id: &str,
        incarnation: ViewIncarnation,
    ) -> String {
        let mut rel = format!("{}/attrs/{column}", self.rel());
        for component in view_path_components(view_id) {
            rel.push('/');
            rel.push_str(component);
        }
        if incarnation != DECLARED_INCARNATION {
            rel.push('@');
            rel.push_str(&incarnation.to_string());
        }
        rel
    }

    /// `entities/unique/<attribute>`: one unique index's runs.
    pub fn unique_index_rel(self, attribute: &str) -> String {
        format!("{}/entities/unique/{attribute}", self.rel())
    }

    /// `entities/edited/<direction>`: the edited-item runs of one direction.
    pub fn edited_runs_rel(self, direction: Direction) -> String {
        format!("{}/entities/edited/{}", self.rel(), direction.name())
    }

    /// `entities/terms`: the entity-to-term transpose's base, with its extents under `extents/`.
    pub fn entity_terms_rel(self) -> String {
        format!("{}/entities/terms", self.rel())
    }

    /// `tenancy`: the tenancy index's files.
    pub fn tenancy_rel(self) -> String {
        format!("{}/tenancy", self.rel())
    }

    /// `members`: the membership extents.
    pub fn members_rel(self) -> String {
        format!("{}/members", self.rel())
    }

    /// `coalesced`: each coalesce writes its own directory in this one.
    pub fn coalesced_rel(self) -> String {
        format!("{}/coalesced", self.rel())
    }

    /// The directory one kind of derived structure is filed in: a `DerivedForm::dir`, or the term
    /// images'.
    pub fn derived_rel(self, kind: &str) -> String {
        format!("{}/{kind}", self.rel())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P0: ShardLayout<'static> = ShardLayout::new("p0", ShardId::ZERO);

    /// A recreated key's scoped column does not land on its predecessor's path, and a
    /// build-declared view's path does not move.
    #[test]
    fn a_scoped_columns_path_moves_with_the_incarnation_and_not_at_the_build() {
        let declared = P0.scoped_column_rel("mood", "quarter:2026-Q3", DECLARED_INCARNATION);
        assert_eq!(
            declared,
            "partitions/p0/shards/0/attrs/mood/quarter/2026-Q3"
        );
        let recreated = P0.scoped_column_rel("mood", "quarter:2026-Q3", 4);
        assert_eq!(
            recreated,
            "partitions/p0/shards/0/attrs/mood/quarter/2026-Q3@4"
        );
        assert_ne!(
            P0.scoped_column_rel("mood", "quarter:2026-Q3", 4),
            P0.scoped_column_rel("mood", "quarter:2026-Q3", 5)
        );
        assert_eq!(
            P0.scoped_column_rel("mood", "world", DECLARED_INCARNATION),
            "partitions/p0/shards/0/attrs/mood/world"
        );
        assert_eq!(
            P0.scoped_column_rel("mood", "world", 2),
            "partitions/p0/shards/0/attrs/mood/world@2"
        );
    }

    #[test]
    fn a_plain_view_is_one_component_and_a_groups_view_is_two() {
        assert_eq!(
            Path::new("/p").join(P0.view_rel("world")),
            Path::new("/p/partitions/p0/shards/0/views/world")
        );
        assert_eq!(
            view_path_components("quarter:2026-Q2"),
            ["quarter", "2026-Q2"]
        );
        assert_eq!(P0.view_rel("world"), "partitions/p0/shards/0/views/world");
        assert_eq!(
            P0.view_rel("quarter:2026-Q2"),
            "partitions/p0/shards/0/views/quarter/2026-Q2"
        );
        assert_eq!(
            P0.segment_rel("quarter:2026-Q2", "seg-1"),
            "partitions/p0/shards/0/views/quarter/2026-Q2/segments/seg-1"
        );
    }
}
