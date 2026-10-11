//! Where a prefix's files live. A `_rel` path is prefix-relative with forward slashes, as a
//! manifest's `files` map keys it.

use std::path::{Path, PathBuf};

use mosaica_types::view::{ViewIncarnation, DECLARED_INCARNATION};
use mosaica_types::ShardId;

use crate::edited::Direction;

pub use mosaica_types::view::GROUP_SEPARATOR;

pub const DICTIONARY_DIR: &str = "dictionary";

/// The directory of the dictionary extent a flush or a coalesce writes, where `id` is the flush's
/// segment id or the coalesce's id.
pub fn dictionary_extent_dir_rel(id: &str) -> String {
    format!("{DICTIONARY_DIR}/{id}")
}

/// The extent's file in [`dictionary_extent_dir_rel`].
pub fn dictionary_extent_rel(id: &str) -> String {
    format!("{}/terms-0.dict", dictionary_extent_dir_rel(id))
}

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

/// One component for a plain view, two for a group's: `:` is not a path character everywhere.
pub fn view_path_components(view_id: &str) -> Vec<&str> {
    match view_id.split_once(GROUP_SEPARATOR) {
        Some((group, key)) => vec![group, key],
        None => vec![view_id],
    }
}

/// Where one point shard's files live.
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
        format!("{}/shards/{}", partition_rel(self.partition), self.shard)
    }

    pub fn dir(self, prefix_dir: &Path) -> PathBuf {
        prefix_dir.join(self.rel())
    }

    /// `views/<view>` or `views/<group>/<key>`.
    pub fn view_rel(self, view_id: &str) -> String {
        format!(
            "{}/views/{}",
            self.rel(),
            view_path_components(view_id).join("/")
        )
    }

    pub fn permutation_rel(self, view_id: &str) -> String {
        format!("{}/permutation.bin", self.view_rel(view_id))
    }

    pub fn segments_rel(self, view_id: &str) -> String {
        format!("{}/segments", self.view_rel(view_id))
    }

    pub fn segment_rel(self, view_id: &str, seg_id: &str) -> String {
        format!("{}/{seg_id}", self.segments_rel(view_id))
    }

    /// `attrs/<column>`: a column's base and extents, or a group-scoped family's views.
    pub fn column_rel(self, column: &str) -> String {
        format!("{}/attrs/{column}", self.rel())
    }

    /// The record blob's base. `record` is reserved out of column names, so no column shares it.
    pub fn record_rel(self) -> String {
        self.column_rel("record")
    }

    pub fn record_extents_rel(self) -> String {
        format!("{}/extents", self.record_rel())
    }

    /// `attrs/<column>/<view components>`: one view's column of a group-scoped family.
    ///
    /// `@<incarnation>` marks the last component above [`DECLARED_INCARNATION`], so a recreated
    /// key's base does not overwrite its predecessor's, which the live manifest may still name. `@`
    /// is reserved out of view keys.
    pub fn scoped_column_rel(
        self,
        column: &str,
        view_id: &str,
        incarnation: ViewIncarnation,
    ) -> String {
        let mut rel = format!(
            "{}/{}",
            self.column_rel(column),
            view_path_components(view_id).join("/")
        );
        if incarnation != DECLARED_INCARNATION {
            rel.push('@');
            rel.push_str(&incarnation.to_string());
        }
        rel
    }

    pub fn terms_rel(self) -> String {
        format!("{}/terms", self.rel())
    }

    pub fn postings_rel(self) -> String {
        format!("{}/postings.arrow", self.terms_rel())
    }

    pub fn pairs_rel(self) -> String {
        format!("{}/pairs.parquet", self.terms_rel())
    }

    pub fn entities_rel(self) -> String {
        format!("{}/entities", self.rel())
    }

    pub fn unique_index_rel(self, attribute: &str) -> String {
        format!("{}/unique/{attribute}", self.entities_rel())
    }

    pub fn edited_runs_rel(self, direction: Direction) -> String {
        format!("{}/edited/{}", self.entities_rel(), direction.name())
    }

    /// The entity-to-term transpose's base. Its extents are under `extents/`.
    pub fn entity_terms_rel(self) -> String {
        format!("{}/terms", self.entities_rel())
    }

    pub fn tenancy_rel(self) -> String {
        format!("{}/tenancy", self.rel())
    }

    pub fn members_rel(self) -> String {
        format!("{}/members", self.rel())
    }

    pub fn coalesced_rel(self, id: &str) -> String {
        format!("{}/coalesced/{id}", self.rel())
    }

    /// The directory one kind of derived structure is filed in: a `DerivedForm::dir`, or the term
    /// images'.
    pub fn derived_rel(self, kind: &str) -> String {
        format!("{}/{kind}", self.rel())
    }
}

#[cfg(test)]
pub(crate) const TEST_SHARD: ShardLayout<'static> = ShardLayout::new("p", ShardId::ZERO);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scoped_columns_path_moves_with_the_incarnation_and_not_at_the_build() {
        let declared =
            TEST_SHARD.scoped_column_rel("mood", "quarter:2026-Q3", DECLARED_INCARNATION);
        assert_eq!(declared, "partitions/p/shards/0/attrs/mood/quarter/2026-Q3");
        let recreated = TEST_SHARD.scoped_column_rel("mood", "quarter:2026-Q3", 4);
        assert_eq!(
            recreated,
            "partitions/p/shards/0/attrs/mood/quarter/2026-Q3@4"
        );
        assert_ne!(
            TEST_SHARD.scoped_column_rel("mood", "quarter:2026-Q3", 4),
            TEST_SHARD.scoped_column_rel("mood", "quarter:2026-Q3", 5)
        );
        assert_eq!(
            TEST_SHARD.scoped_column_rel("mood", "world", DECLARED_INCARNATION),
            "partitions/p/shards/0/attrs/mood/world"
        );
        assert_eq!(
            TEST_SHARD.scoped_column_rel("mood", "world", 2),
            "partitions/p/shards/0/attrs/mood/world@2"
        );
    }

    #[test]
    fn a_plain_view_is_one_component_and_a_groups_view_is_two() {
        assert_eq!(
            Path::new("/p").join(TEST_SHARD.view_rel("world")),
            Path::new("/p/partitions/p/shards/0/views/world")
        );
        assert_eq!(
            view_path_components("quarter:2026-Q2"),
            ["quarter", "2026-Q2"]
        );
        assert_eq!(
            TEST_SHARD.view_rel("world"),
            "partitions/p/shards/0/views/world"
        );
        assert_eq!(
            TEST_SHARD.view_rel("quarter:2026-Q2"),
            "partitions/p/shards/0/views/quarter/2026-Q2"
        );
        assert_eq!(
            TEST_SHARD.segment_rel("quarter:2026-Q2", "seg-1"),
            "partitions/p/shards/0/views/quarter/2026-Q2/segments/seg-1"
        );
    }
}
