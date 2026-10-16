//! An empty point shard's base: every declared structure and column, holding no item.
//!
//! A build over no items writes its one shard with [`write_empty_shard`], and a running service
//! writes a shard it opens with it, so a reader never meets a shard without a base and the two
//! paths cannot write different files for the same declarations.

use std::path::{Path, PathBuf};

use mosaica_spatial::tiler::ScalarType;
use mosaica_types::view::ViewIncarnation;

use crate::error::{io, Result};
use crate::manifest::{DeclaredScalar, ManifestVocabulary, ScopedScalar, SegmentDescriptor};
use crate::ShardLayout;

/// The declarations an empty shard's base is shaped by.
pub struct EmptyShard<'a> {
    pub prefix_dir: &'a Path,
    pub layout: ShardLayout<'a>,
    /// The dictionary's length: the postings hold one empty record for each term.
    pub terms: u64,
    pub small_term_threshold: u32,
    /// Whether `terms/pairs.parquet` is written beside the postings.
    pub pairs: bool,
    pub declared: &'a [DeclaredScalar],
    pub vocabularies: &'a [ManifestVocabulary],
    /// Every group's scoped families. A family's column is written in each of its views that
    /// [`EmptyShard::views`] holds.
    pub scoped: &'a [ScopedScalar],
    pub views: &'a [EmptyView<'a>],
    /// The base segment's id in every view.
    pub seg_id: &'a str,
}

/// One view of an empty shard.
pub struct EmptyView<'a> {
    pub id: &'a str,
    pub incarnation: ViewIncarnation,
    /// `columns.arrow`'s tail: the rendered columns, then this view's scoped render lanes.
    pub render: Vec<(String, ScalarType)>,
}

/// What [`write_empty_shard`] wrote.
pub struct EmptyShardWritten {
    /// Every file, prefix-relative, with its path.
    pub files: Vec<(String, PathBuf)>,
    /// One base segment of no rows for each view, in [`EmptyShard::views`]' order.
    pub segments: Vec<SegmentDescriptor>,
}

/// The kind a value column of `arrow_type` is written as: a keyword's ordinals and a text
/// column's terms as `u32`, a category's codes at its width, and a number at its own.
pub fn value_column_kind(arrow_type: ScalarType, category: bool) -> mosaica_filter::ColumnKind {
    use mosaica_filter::ColumnKind;
    match arrow_type {
        ScalarType::Keyword | ScalarType::Text | ScalarType::Utf8 => ColumnKind::U32,
        ScalarType::U8 => ColumnKind::U8,
        ScalarType::U16 => ColumnKind::U16,
        _ if category => ColumnKind::U32,
        ScalarType::Bool => ColumnKind::U8,
        ScalarType::U32 => ColumnKind::U32,
        ScalarType::U64 => ColumnKind::U64,
        ScalarType::I8 => ColumnKind::I8,
        ScalarType::I16 => ColumnKind::I16,
        ScalarType::I32 => ColumnKind::I32,
        ScalarType::I64 | ScalarType::TimestampUs => ColumnKind::I64,
        ScalarType::F32 => ColumnKind::F32,
        ScalarType::F64 => ColumnKind::F64,
    }
}

/// A value column of no entity in `dir`, the base of a column no layer holds a value of: its
/// values, which name no presence bitmap, and for a keyword its empty dictionary. Returns the
/// files written.
pub fn write_empty_value_column(
    dir: &Path,
    arrow_type: ScalarType,
    category: bool,
) -> Result<Vec<PathBuf>> {
    let values = dir.join(mosaica_filter::VALUES_FILE);
    mosaica_filter::ValueColumnWriter::create(
        &values,
        &dir.join(mosaica_filter::PRESENCE_FILE),
        value_column_kind(arrow_type, category),
    )
    .and_then(|writer| writer.finish(None))
    .map_err(io(&values))?;
    let mut written = vec![values];
    if arrow_type == ScalarType::Keyword {
        let path = dir.join(mosaica_filter::DICT_FILE);
        write_empty_dictionary(&path)?;
        written.push(path);
    }
    Ok(written)
}

/// A sorted dictionary of no term at `path`.
fn write_empty_dictionary(path: &Path) -> Result<()> {
    let file = std::fs::File::create(path).map_err(io(path))?;
    mosaica_filter::SortedDictWriter::new(std::io::BufWriter::new(file))
        .and_then(|writer| writer.finish())
        .map_err(|e| io(path)(std::io::Error::from(e)))?;
    Ok(())
}

/// Write an empty base of every structure `shard` declares under its layout: the postings, the
/// entity-to-term transpose, each value column with its dictionary and postings, each indexed
/// text column's index, the record blob, each view's columns of every scoped family, and each
/// view's segment, permutation, row-to-entity table and field tallies. The unique, edited-items
/// and tenancy indexes of an empty shard hold no run. Every file is fsynced.
pub fn write_empty_shard(shard: &EmptyShard<'_>) -> Result<EmptyShardWritten> {
    let mut out = Written {
        prefix_dir: shard.prefix_dir,
        files: Vec::new(),
    };
    let layout = shard.layout;

    let terms_dir = shard.prefix_dir.join(layout.terms_rel());
    std::fs::create_dir_all(&terms_dir).map_err(io(&terms_dir))?;
    let postings_rel = layout.postings_rel();
    let postings_path = out.path(&postings_rel);
    let spool_path = terms_dir.join("postings.spool");
    let mut spool =
        mosaica_authz::postings::PostingsSpool::create(&spool_path).map_err(io(&spool_path))?;
    let empty = mosaica_authz::postings::encode_posting(0, &[], shard.small_term_threshold)
        .map_err(io(&postings_path))?;
    for _ in 0..shard.terms {
        spool.append(&empty).map_err(io(&spool_path))?;
    }
    spool.finish(&postings_path).map_err(io(&postings_path))?;
    out.push(postings_rel);
    if shard.pairs {
        let pairs_rel = layout.pairs_rel();
        crate::PairsParquetWriter::create(&out.path(&pairs_rel))?.finish()?;
        out.push(pairs_rel);
    }

    let entity_terms_rel = layout.entity_terms_rel();
    for path in crate::EntityTermsWriter::create(&out.path(&entity_terms_rel))?.finish()? {
        out.wrote(&entity_terms_rel, path);
    }

    for scalar in shard.declared {
        let column_rel = layout.column_rel(&scalar.name);
        if scalar.arrow_type == ScalarType::Text {
            if scalar.index {
                out.text_index(&column_rel)?;
            }
        } else if scalar.owes_value_column(shard.vocabularies) {
            out.value_column(
                &column_rel,
                scalar.arrow_type,
                scalar.vocabulary.is_some(),
                scalar.owes_postings(shard.vocabularies),
            )?;
        }
        if scalar.unique {
            let dir = out.path(&layout.unique_index_rel(&scalar.name));
            std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        }
    }
    if shard
        .declared
        .iter()
        .any(|scalar| scalar.blob_resident(shard.vocabularies))
    {
        out.record_blob(&layout.record_rel())?;
    }

    for family in shard.scoped {
        for view in &family.views {
            let Some(at) = shard.views.iter().find(|v| v.id == view) else {
                continue;
            };
            let column_rel = layout.scoped_column_rel(&family.name, view, at.incarnation);
            if family.has_value_column() {
                out.value_column(
                    &column_rel,
                    family.arrow_type,
                    family.vocabulary.is_some(),
                    family.vocabulary.is_some() && family.is_filterable(),
                )?;
            } else {
                if family.index {
                    out.text_index(&column_rel)?;
                }
                out.record_blob(&format!(
                    "{column_rel}/{}",
                    crate::manifest::SCOPED_PROSE_DIR
                ))?;
            }
        }
    }

    let indexed: Vec<(String, ScalarType)> = shard
        .declared
        .iter()
        .filter(|scalar| scalar.band_copied())
        .map(|scalar| (scalar.name.clone(), scalar.arrow_type))
        .collect();
    let mut segments = Vec::with_capacity(shard.views.len());
    for view in shard.views {
        out.row_space(layout, view, shard.seg_id, &indexed)?;
        let tallies = crate::field_tallies::derive_view(
            shard.prefix_dir,
            layout,
            view.id,
            shard.seg_id,
            0,
            shard.declared,
            &|name| mosaica_filter::base_numbers(&shard.prefix_dir.join(layout.column_rel(name))),
        )?;
        out.wrote(&layout.view_rel(view.id), tallies);
        segments.push(SegmentDescriptor {
            view: view.id.to_string(),
            incarnation: view.incarnation,
            seg_id: shard.seg_id.to_string(),
            row_count: 0,
            entity_lo: 0,
            entity_hi: 0,
        });
    }

    let paths: Vec<PathBuf> = out.files.iter().map(|(_, path)| path.clone()).collect();
    crate::fsync_written(&paths)?;
    Ok(EmptyShardWritten {
        files: out.files,
        segments,
    })
}

struct Written<'a> {
    prefix_dir: &'a Path,
    files: Vec<(String, PathBuf)>,
}

impl Written<'_> {
    fn path(&self, rel: &str) -> PathBuf {
        self.prefix_dir.join(rel)
    }

    fn push(&mut self, rel: String) {
        let path = self.path(&rel);
        self.files.push((rel, path));
    }

    /// A file written by name under `dir_rel`.
    fn wrote(&mut self, dir_rel: &str, path: PathBuf) {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        self.files.push((format!("{dir_rel}/{name}"), path));
    }

    fn dir(&self, rel: &str) -> Result<PathBuf> {
        let dir = self.path(rel);
        std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        Ok(dir)
    }

    /// A value column of no entity, with its empty dictionary for a keyword and its empty postings
    /// for a category that owes them. It names no presence bitmap.
    fn value_column(
        &mut self,
        column_rel: &str,
        arrow_type: ScalarType,
        category: bool,
        postings: bool,
    ) -> Result<()> {
        let dir = self.dir(column_rel)?;
        for path in write_empty_value_column(&dir, arrow_type, category)? {
            self.wrote(column_rel, path);
        }
        if postings {
            let path = dir.join("postings.arrow");
            mosaica_filter_write::write_category_postings(
                &path,
                column_rel,
                &NoCodes,
                mosaica_filter_write::POSTINGS_BAND_ROWS,
            )
            .map_err(io(&path))?;
            self.push(format!("{column_rel}/postings.arrow"));
        }
        Ok(())
    }

    fn dictionary(&mut self, column_rel: &str) -> Result<()> {
        write_empty_dictionary(&self.path(&format!("{column_rel}/{}", mosaica_filter::DICT_FILE)))?;
        self.push(format!("{column_rel}/{}", mosaica_filter::DICT_FILE));
        Ok(())
    }

    /// A text column's index of no term: its dictionary and its postings.
    fn text_index(&mut self, column_rel: &str) -> Result<()> {
        let dir = self.dir(column_rel)?;
        self.dictionary(column_rel)?;
        let postings = dir.join("postings.arrow");
        let spool = postings.with_extension("spool");
        mosaica_authz::postings::PostingsSpool::create(&spool)
            .and_then(|spool| spool.finish(&postings))
            .map_err(io(&postings))?;
        self.push(format!("{column_rel}/postings.arrow"));
        Ok(())
    }

    /// A record blob of no row under `dir_rel`.
    fn record_blob(&mut self, dir_rel: &str) -> Result<()> {
        let dir = self.dir(dir_rel)?;
        let names = [
            mosaica_filter::RECORD_BLOCKS_FILE,
            mosaica_filter::RECORD_HASROW_FILE,
            mosaica_filter::RECORD_DIRECTORY_FILE,
        ];
        let [blocks, hasrow, directory] = names.map(|name| dir.join(name));
        mosaica_filter_write::RecordBlobWriter::create(
            &blocks,
            &hasrow,
            &directory,
            mosaica_filter::RECORD_BLOCK_TARGET,
        )
        .and_then(|writer| writer.finish())
        .map_err(io(&blocks))?;
        for name in names {
            self.push(format!("{dir_rel}/{name}"));
        }
        Ok(())
    }

    /// One view's segment of no rows, its permutation over no entity and its empty row-to-entity
    /// table.
    fn row_space(
        &mut self,
        layout: ShardLayout<'_>,
        view: &EmptyView<'_>,
        seg_id: &str,
        indexed: &[(String, ScalarType)],
    ) -> Result<()> {
        let view_rel = layout.view_rel(view.id);
        let segment_rel = layout.segment_rel(view.id, seg_id);
        let segment_dir = self.dir(&segment_rel)?;

        let morton = segment_dir.join("morton.u32");
        std::fs::File::create(&morton).map_err(io(&morton))?;
        crate::write::CutWriter::create(&segment_dir)
            .and_then(|cuts| cuts.finish())
            .map_err(io(&segment_dir))?;
        let columns = segment_dir.join("columns.arrow");
        crate::columns::ColumnsPlan::new(&view.render, 0)
            .and_then(|plan| crate::columns::ColumnsFile::create(&columns, plan))
            .and_then(|file| file.finish())
            .map_err(io(&columns))?;
        crate::bands::BandWriter::create(&segment_dir, indexed)
            .and_then(|bands| bands.finish(&segment_dir))
            .map_err(io(&segment_dir))?;
        for name in crate::SEGMENT_FILES {
            self.push(format!("{segment_rel}/{name}"));
        }

        let permutation_rel = layout.permutation_rel(view.id);
        let permutation = self.path(&permutation_rel);
        crate::write::PagePlan::new(0)
            .and_then(|plan| crate::write::PermutationWriter::create_planned(&permutation, &plan))
            .and_then(|writer| writer.finish())
            .map_err(io(&permutation))?;
        self.push(permutation_rel);
        let row_entity_rel = format!("{view_rel}/{}", crate::ROW_ENTITY_FILE);
        let row_entity = self.path(&row_entity_rel);
        std::fs::File::create(&row_entity).map_err(io(&row_entity))?;
        self.push(row_entity_rel);
        Ok(())
    }
}

/// A category column of no entity, as its postings' source.
struct NoCodes;

impl mosaica_filter_write::CategorySource for NoCodes {
    fn for_each(
        &self,
        _visit: &mut dyn FnMut(u32, u32) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        Ok(())
    }
}
