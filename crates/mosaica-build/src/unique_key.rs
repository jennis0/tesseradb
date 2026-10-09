//! An indexed keyword's cardinality against the rows that carry it, and what its index cost.
//!
//! A keyword column with `index = true` is stored as a sorted dictionary of its distinct values
//! and a `u32` ordinal per present row (`records-and-search.md` §4.3). Where every row carries a
//! different value, the dictionary holds every value once and the column is a second copy of the
//! source column plus four bytes a row: the index answers a lookup by that one key and nothing
//! else. Rung 5 of the ingest campaign indexed a 36-byte `uuid` over 233 million rows, and the
//! column was 8 GB of a 40 GB bundle (`docs/ingest-campaign.md` §4b).
//!
//! **A warning, never a refusal.** A unique key indexed for the lookup is legitimate, and the
//! build cannot tell that intent from an oversight. The figures are printed with their
//! denominators, at the build and at `tessera verify`, and the operator decides.
//!
//! The denominator is the rows that carry a value, not every row: a key unique wherever it is
//! present is a unique key, and the index costs in proportion to the rows present. Both counts
//! are printed beside the bundle's row count.
//!
//! `tessera check` reads Parquet footers and no row, so it can say this only where the writer
//! recorded a distinct count in the column's statistics. Most writers do not, and the check is
//! then silent about it; the build's report is where the figure is certain.

use std::collections::BTreeMap;
use std::path::Path;

use tessera_filter::{Access, SortedDict, ValueColumn};
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{FileDigest, Manifest, SegmentsManifest};

use crate::error::{BuildError, Result};

/// Distinct keys at or above this fraction of the present rows is reported as a unique key.
///
/// "Within a few per cent": a key with one duplicate in thirty is still, for the index's cost, a
/// unique key.
pub const UNIQUE_KEY_FRACTION: f64 = 0.97;

/// One indexed keyword column's figures, read from the bundle it was written into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordCardinality {
    pub attribute: String,
    /// The view whose column this is, for a group-scoped family; `None` for an entity-scoped
    /// attribute, whose one column covers every entity.
    pub view: Option<String>,
    /// Keys in the column's dictionary.
    pub distinct: u64,
    /// Rows carrying a value in this column.
    pub present: u64,
    /// Rows the column could have covered: the build's entities, or the view's rows for a scoped
    /// family.
    pub rows: u64,
    /// The dictionary, the ordinal column and the presence bitmap together, as the manifest
    /// digests them.
    pub index_bytes: u64,
}

impl KeywordCardinality {
    /// Whether the distinct count is within [`UNIQUE_KEY_FRACTION`] of the present rows.
    pub fn is_unique_key(&self) -> bool {
        self.present > 0 && self.distinct as f64 >= self.present as f64 * UNIQUE_KEY_FRACTION
    }

    fn label(&self) -> String {
        match &self.view {
            Some(view) => format!("attribute '{}' in view '{view}'", self.attribute),
            None => format!("attribute '{}'", self.attribute),
        }
    }

    fn present_fraction(&self) -> f64 {
        if self.present == 0 {
            0.0
        } else {
            self.distinct as f64 / self.present as f64
        }
    }

    /// The figures, printed for every indexed keyword so the denominator is always in view.
    pub fn report(&self) -> String {
        format!(
            "{}: {} distinct key(s) over {} row(s) with a value, of {} row(s); the index is {}",
            self.label(),
            crate::thousands(self.distinct),
            crate::thousands(self.present),
            crate::thousands(self.rows),
            human_bytes(self.index_bytes)
        )
    }

    /// The emphatic line a unique key earns, with the bundle's total as the denominator. `None`
    /// where the key repeats.
    pub fn warning(&self, bundle_bytes: u64) -> Option<String> {
        if !self.is_unique_key() {
            return None;
        }
        let share = if bundle_bytes == 0 {
            0.0
        } else {
            self.index_bytes as f64 / bundle_bytes as f64 * 100.0
        };
        Some(format!(
            "{}: UNIQUE KEY INDEXED. {} distinct key(s) over {} row(s) with a value ({:.1}%), so \
             the dictionary holds every value once and the index is {} of the {} bundle ({share:.1}%). \
             An index over a unique key answers a lookup by that key and nothing else. Reported, \
             not refused: a key indexed for that lookup is meant. Remove `index = true` from the \
             attribute if it is not",
            self.label(),
            crate::thousands(self.distinct),
            crate::thousands(self.present),
            self.present_fraction() * 100.0,
            human_bytes(self.index_bytes),
            human_bytes(bundle_bytes),
        ))
    }
}

/// Every indexed keyword column the bundle at `prefix_dir` holds, entity-scoped attributes first
/// and then each group-scoped family per view, read from the files the manifests name.
///
/// `partitions` pairs each partition's hash with its side-manifest. A column whose dictionary or
/// value column does not open is an error: the build has just written it, and a verify that could
/// not read it is looking at a malformed bundle.
pub fn keyword_cardinalities(
    prefix_dir: &Path,
    manifest: &Manifest,
    partitions: &[(&str, &SegmentsManifest)],
) -> Result<Vec<KeywordCardinality>> {
    let mut out = Vec::new();
    for (phash, segments) in partitions {
        let attrs_rel = format!("partitions/{phash}/attrs");
        for scalar in &manifest.declared_scalars {
            if scalar.arrow_type != ScalarType::Keyword || !scalar.index {
                continue;
            }
            let dir_rel = format!("{attrs_rel}/{}", scalar.name);
            let (distinct, present) = read_column(&prefix_dir.join(&dir_rel))?;
            out.push(KeywordCardinality {
                attribute: scalar.name.clone(),
                view: None,
                distinct,
                present,
                rows: manifest.entity_id_high_water,
                index_bytes: bytes_under(&dir_rel, &[&manifest.files, &segments.files]),
            });
        }
        for group in &manifest.groups {
            for scalar in &group.scoped_scalars {
                if scalar.arrow_type != ScalarType::Keyword || !scalar.index {
                    continue;
                }
                for view in &scalar.views {
                    let mut dir_rel = format!("{attrs_rel}/{}", scalar.name);
                    for component in tessera_store::view_path_components(view) {
                        dir_rel.push('/');
                        dir_rel.push_str(component);
                    }
                    let (distinct, present) = read_column(&prefix_dir.join(&dir_rel))?;
                    let rows = segments
                        .segments
                        .iter()
                        .filter(|segment| &segment.view == view)
                        .map(|segment| segment.row_count as u64)
                        .sum();
                    out.push(KeywordCardinality {
                        attribute: scalar.name.clone(),
                        view: Some(view.clone()),
                        distinct,
                        present,
                        rows,
                        index_bytes: bytes_under(&dir_rel, &[&manifest.files, &segments.files]),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Print each column's figures and the warning a unique key earns, as the build's report does
/// for attribute coverage.
pub(crate) fn report_keyword_cardinalities(columns: &[KeywordCardinality], bundle_bytes: u64) {
    for column in columns {
        eprintln!("{}", column.report());
        if let Some(warning) = column.warning(bundle_bytes) {
            eprintln!("{warning}");
        }
    }
}

/// `(distinct, present)` from the dictionary's key count and the value column's presence. Both are
/// mapped, and only the dictionary's footer and the presence bitmap are read.
fn read_column(dir: &Path) -> Result<(u64, u64)> {
    let dict = SortedDict::open_dir(dir, Access::Mapped).map_err(|e| {
        BuildError::io(
            &dir.join(tessera_filter::DICT_FILE),
            std::io::Error::from(e),
        )
    })?;
    let values = ValueColumn::open_dir(dir, Access::Mapped).map_err(|e| BuildError::io(dir, e))?;
    Ok((dict.len() as u64, values.present().cardinality()))
}

/// The sizes of the files directly under `dir_rel`, as the manifests digest them. Files in a
/// subdirectory are another column's: a group-scoped family's per-view directories sit under the
/// attribute's own.
fn bytes_under(dir_rel: &str, files: &[&BTreeMap<String, FileDigest>]) -> u64 {
    let prefix = format!("{dir_rel}/");
    files
        .iter()
        .flat_map(|map| map.iter())
        .filter(|(path, _)| {
            path.strip_prefix(&prefix)
                .is_some_and(|rest| !rest.contains('/'))
        })
        .map(|(_, digest)| digest.size)
        .sum()
}

/// A byte count at the unit an operator compares by eye, decimal, as the campaign's figures are.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// What a Parquet footer records about one column's distinct values, summed over its row groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterCount {
    /// The writer's distinct counts, summed. A value present in two row groups is counted twice,
    /// so this is an upper bound on the column's cardinality.
    pub distinct: u64,
    /// Non-null values across the same row groups.
    pub values: u64,
    pub row_groups: usize,
}

impl FooterCount {
    /// Whether the summed distinct count is within [`UNIQUE_KEY_FRACTION`] of the values.
    pub fn is_unique_key(&self) -> bool {
        self.values > 0 && self.distinct as f64 >= self.values as f64 * UNIQUE_KEY_FRACTION
    }

    /// The line `tessera check` prints for an indexed keyword whose footer says it is unique.
    pub fn warning(&self) -> Option<String> {
        if !self.is_unique_key() {
            return None;
        }
        Some(format!(
            "UNIQUE KEY INDEXED, by the source's footer: {} distinct value(s) over {} non-null \
             row(s) ({:.1}%) in {} row group(s). The counts are the writer's, summed over row \
             groups, so this is an upper bound; the build reports the exact cardinality and the \
             bytes the index costs. An index over a unique key answers a lookup by that key and \
             nothing else. Remove `index = true` from the attribute if that lookup is not wanted",
            crate::thousands(self.distinct),
            crate::thousands(self.values),
            self.distinct as f64 / self.values as f64 * 100.0,
            self.row_groups
        ))
    }
}

/// The footer's distinct count for `column` in the Parquet file at `path`, or `None` where any
/// row group's statistics omit it. Reads the footer only.
pub fn footer_distinct_count(
    path: &Path,
    column: &str,
) -> std::result::Result<Option<FooterCount>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("{}: not readable as Parquet: {e}", path.display()))?;
    let metadata = builder.metadata();
    let Some(leaf) = metadata
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .position(|descriptor| descriptor.name() == column)
    else {
        return Ok(None);
    };
    let mut count = FooterCount {
        distinct: 0,
        values: 0,
        row_groups: 0,
    };
    for row_group in metadata.row_groups() {
        let Some(statistics) = row_group.column(leaf).statistics() else {
            return Ok(None);
        };
        let Some(distinct) = statistics.distinct_count_opt() else {
            return Ok(None);
        };
        let nulls = statistics.null_count_opt().unwrap_or(0);
        count.distinct += distinct;
        count.values += (row_group.num_rows().max(0) as u64).saturating_sub(nulls);
        count.row_groups += 1;
    }
    if count.row_groups == 0 {
        return Ok(None);
    }
    Ok(Some(count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(distinct: u64, present: u64, rows: u64, index_bytes: u64) -> KeywordCardinality {
        KeywordCardinality {
            attribute: "uuid".into(),
            view: None,
            distinct,
            present,
            rows,
            index_bytes,
        }
    }

    /// The threshold is a fraction of the rows present, not of every row: a key unique wherever
    /// it is present is reported, and one duplicate in thirty still is.
    #[test]
    fn a_unique_key_is_within_a_few_per_cent_of_the_present_rows() {
        assert!(column(100, 100, 100, 1).is_unique_key());
        assert!(column(75, 75, 100, 1).is_unique_key());
        assert!(column(98, 100, 100, 1).is_unique_key());
        assert!(!column(96, 100, 100, 1).is_unique_key());
        assert!(!column(3, 100, 100, 1).is_unique_key());
        assert!(!column(0, 0, 100, 1).is_unique_key());
    }

    /// The warning carries the index's share of the bundle, and a repeating key earns none.
    #[test]
    fn the_warning_states_the_share_of_the_bundle() {
        let warning = column(233_055_986, 233_055_986, 233_055_986, 8_000_000_000)
            .warning(40_000_000_000)
            .expect("a unique key warns");
        assert!(warning.contains("UNIQUE KEY INDEXED"), "{warning}");
        assert!(
            warning.contains("8.00 GB of the 40.00 GB bundle (20.0%)"),
            "{warning}"
        );
        assert!(warning.contains("233,055,986 distinct key(s)"), "{warning}");
        assert_eq!(column(474, 233_055_986, 233_055_986, 1).warning(40), None);
    }

    #[test]
    fn bytes_are_summed_for_the_column_directory_alone() {
        let mut files = BTreeMap::new();
        for (path, size) in [
            ("partitions/default/attrs/uuid/dict.bin", 700u64),
            ("partitions/default/attrs/uuid/values.arrow", 200),
            ("partitions/default/attrs/uuid/presence.roaring", 30),
            ("partitions/default/attrs/uuid/q/2026/dict.bin", 5000),
            ("partitions/default/attrs/uuid2/dict.bin", 9000),
            ("partitions/default/attrs/name/dict.bin", 11),
        ] {
            files.insert(
                path.to_string(),
                FileDigest {
                    size,
                    sha256: String::new(),
                },
            );
        }
        assert_eq!(bytes_under("partitions/default/attrs/uuid", &[&files]), 930);
        assert_eq!(
            bytes_under("partitions/default/attrs/uuid/q/2026", &[&files]),
            5000
        );
    }

    #[test]
    fn bytes_read_at_the_unit_an_operator_compares() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_000), "1.00 KB");
        assert_eq!(human_bytes(8_012_345_678), "8.01 GB");
        assert_eq!(human_bytes(39_970_000_000), "39.97 GB");
    }

    /// The footer's rule is the same fraction; a footer with no distinct count says nothing.
    #[test]
    fn a_footer_count_warns_on_the_same_fraction() {
        let unique = FooterCount {
            distinct: 1_000,
            values: 1_000,
            row_groups: 3,
        };
        assert!(unique.is_unique_key());
        let warning = unique.warning().unwrap();
        assert!(warning.contains("upper bound"), "{warning}");
        let repeating = FooterCount {
            distinct: 15,
            values: 1_000,
            row_groups: 3,
        };
        assert_eq!(repeating.warning(), None);
    }
}
