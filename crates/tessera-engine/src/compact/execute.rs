use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tessera_authz::{sweep_term_postings, DeltaTier, PostingsReader, PostingsSpool};
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{DeclaredScalar, FileDigest, ManifestVocabulary, SegmentDescriptor};
use tessera_store::render_presence::RENDER_PRESENCE_DIR;
use tessera_store::{
    fold_external_id_runs, fold_row_space, FoldRowSpaceSpec, FoldSegmentInput, PairsParquetWriter,
};
use tessera_types::IdentityKey;

use crate::flush::MaintenanceFailed;

use super::attributes::{
    fold_entity_terms, fold_record_blob, fold_text_columns, fold_value_columns,
};
use super::cost::{PassCost, Staircase};
use super::plan::{FoldPlan, TERM_IMAGE_MANIFEST_N, TERM_IMAGE_THREADS};

/// Everything [`execute`] needs beyond its plan, taken from the generation and then immutable.
/// The two `Arc`s are the live readers, so the fold reads the mappings requests are served from.
pub(crate) struct FoldContext {
    pub(crate) from_prefix_dir: PathBuf,
    pub(crate) to_prefix: String,
    pub(crate) to_prefix_dir: PathBuf,
    pub(crate) identity_key: IdentityKey,
    pub(crate) shard_id: u32,
    /// Per view: the bundle-wide render tail plus that view's group-scoped render lanes.
    pub(crate) scalar_schema: BTreeMap<String, Vec<(String, ScalarType)>>,
    /// Per view, the columns a segment may lack; any other missing column is a torn segment.
    pub(crate) absent_ok: BTreeMap<String, Vec<String>>,
    /// Entity-scoped columns declared at a running service and not yet folded.
    pub(crate) runtime_attributes: Vec<String>,
    pub(crate) runtime_scoped_attributes: Vec<String>,
    /// The new base segment's id in every view. Never reused.
    pub(crate) seg_id: String,
    pub(crate) base_postings: Arc<PostingsReader>,
    pub(crate) tiers: Vec<Arc<DeltaTier>>,
    pub(crate) declared_scalars: Vec<DeclaredScalar>,
    /// Every group's scoped column families, flattened. Each owes a folded column per view.
    pub(crate) scoped_scalars: Vec<tessera_store::manifest::ScopedScalar>,
    /// A scoped column's directory names its view's incarnation.
    pub(crate) view_incarnations:
        std::collections::HashMap<String, tessera_types::view::ViewIncarnation>,
    pub(crate) vocabularies: Vec<ManifestVocabulary>,
}

/// The summary is carried to publication's log, since its timings and counts are not in the file.
pub(crate) struct FoldedTermImages {
    pub(crate) extent: tessera_store::manifest::TermImageExtent,
    pub(crate) summary: tessera_store::term_images::TermImageSummary,
}

/// A fold whose files are durable under a prefix nothing yet names.
pub(crate) struct CompletedFold {
    pub(crate) plan: FoldPlan,
    pub(crate) prefix: String,
    pub(crate) segments: Vec<SegmentDescriptor>,
    /// The files the fold wrote. Publication adds the carried files' digests for `MANIFEST.json`.
    pub(crate) files: BTreeMap<String, FileDigest>,
    /// Run 0's path, or `None` when the deployment holds no external ids.
    pub(crate) external_id_run: Option<String>,
    /// Written unchanged into the new `SEGMENTS-<n>.json`.
    pub(crate) term_images: Vec<FoldedTermImages>,
    pub(crate) cost: Vec<PassCost>,
    /// Publication's first cost row is measured from here.
    pub(crate) finished: std::time::Instant,
    pub(crate) attr_bytes_read: u64,
    pub(crate) attr_bytes_written: u64,
    /// The columns this fold gave a base, which publication takes off the runtime lists.
    pub(crate) runtime_attributes: Vec<String>,
    pub(crate) runtime_scoped_attributes: Vec<String>,
}

#[derive(Default)]
pub(super) struct FoldOutput {
    /// Every file the fold writes, in order. Pass 5 digests exactly this list, so a file not
    /// recorded here is missing from the new `MANIFEST.json`.
    written: Vec<(String, PathBuf)>,
    attr_read: u64,
    attr_written: u64,
}

impl FoldOutput {
    fn push(&mut self, rel: String, path: PathBuf) {
        self.written.push((rel, path));
    }

    pub(super) fn wrote(&mut self, rel: String, path: PathBuf) {
        self.attr_written += file_len(&path);
        self.written.push((rel, path));
    }

    pub(super) fn read(&mut self, dir: &Path, files: impl IntoIterator<Item = impl AsRef<Path>>) {
        for file in files {
            self.attr_read += file_len(&dir.join(file));
        }
    }
}

pub(super) fn failed(what: &str, e: &dyn std::fmt::Display) -> MaintenanceFailed {
    MaintenanceFailed(format!("{what}: {e}"))
}

/// Zero where the file cannot be read: the figure is only reported.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Removes the spool if `outcome` failed. On success the writer's `finish` has removed it.
pub(super) fn remove_spool_on_error<T, E>(outcome: Result<T, E>, spool: &Path) -> Result<T, E> {
    if outcome.is_err() {
        let _ = std::fs::remove_file(spool);
    }
    outcome
}

/// Runs the fold's passes into `ctx.to_prefix_dir` on one dedicated thread. A failure discards the
/// fold: its files are orphans under a prefix `CURRENT` does not name, and there is no resume.
pub(crate) fn execute(
    plan: FoldPlan,
    ctx: FoldContext,
) -> Result<CompletedFold, MaintenanceFailed> {
    let partition_dir = ctx.to_prefix_dir.join("partitions").join(&plan.partition);
    let terms_dir = partition_dir.join("terms");
    let entities_dir = partition_dir.join("entities");
    for dir in [&terms_dir, &entities_dir] {
        std::fs::create_dir_all(dir).map_err(|e| failed("creating the new prefix", &e))?;
    }

    let mut out = FoldOutput::default();

    let mut stairs = Staircase::start();
    stairs.record("entry");

    let segments = fold_row_spaces(&plan, &ctx, &mut out)?;
    stairs.record("1 row space");

    let postings_path = fold_postings(&plan, &ctx, &terms_dir, &mut out)?;
    stairs.record("2 postings");

    let term_images = derive_term_images(&plan, &ctx, &segments, &postings_path, &mut out)?;
    stairs.record("2b term images");

    let external_id_run = fold_external_ids(&plan, &ctx, &entities_dir, &mut out)?;
    stairs.record("3 external ids");

    fold_text_columns(&plan, &ctx, &mut out)?;
    fold_value_columns(&plan, &ctx, &mut out)?;
    fold_record_blob(&plan, &ctx, &mut out)?;
    stairs.record("4a attributes");

    fold_entity_terms(&plan, &ctx, &mut out)?;
    stairs.record("4c entity terms");

    // No pass 4b: the term dictionary is carried to the new prefix by a hard link at publication.
    let files = digest_and_sync(&out)?;
    stairs.record("5 digests + fsync");

    let finished = stairs.mark();
    Ok(CompletedFold {
        plan,
        prefix: ctx.to_prefix,
        segments,
        files,
        external_id_run,
        term_images,
        cost: stairs.into_cost(),
        finished,
        attr_bytes_read: out.attr_read,
        attr_bytes_written: out.attr_written,
        runtime_attributes: ctx.runtime_attributes,
        runtime_scoped_attributes: ctx.runtime_scoped_attributes,
    })
}

/// Pass 1: a new base segment and `permutation.bin` per view. Tombstoned rows are dropped, which
/// shifts every later row id. Returns the descriptors.
fn fold_row_spaces(
    plan: &FoldPlan,
    ctx: &FoldContext,
    output: &mut FoldOutput,
) -> Result<Vec<SegmentDescriptor>, MaintenanceFailed> {
    let mut segments: Vec<SegmentDescriptor> = Vec::with_capacity(plan.views.len());
    for view in &plan.views {
        // An empty schema would write a segment with no scalar tail.
        let Some(view_schema) = ctx.scalar_schema.get(&view.view) else {
            return Err(MaintenanceFailed(format!(
                "pass 1 (row space): the fold plan names view '{}' but the fold has no writer \
                 schema for it",
                view.view
            )));
        };
        let view_rel = format!(
            "partitions/{}/{}",
            plan.partition,
            tessera_store::view_rel(&view.view)
        );
        let view_dir = ctx.to_prefix_dir.join(&view_rel);
        std::fs::create_dir_all(&view_dir).map_err(|e| failed("creating the view", &e))?;
        let segment_rel = format!("{view_rel}/segments/{}", ctx.seg_id);
        let segment_dir = ctx.to_prefix_dir.join(&segment_rel);
        let permutation_rel = format!("{view_rel}/permutation.bin");
        let permutation_path = ctx.to_prefix_dir.join(&permutation_rel);
        let row_entity_rel = format!("{view_rel}/{}", tessera_store::ROW_ENTITY_FILE);
        let row_entity_path = ctx.to_prefix_dir.join(&row_entity_rel);

        let inputs: Vec<FoldSegmentInput> = view
            .segments
            .iter()
            .map(|planned| FoldSegmentInput {
                seg_id: planned.seg_id.clone(),
                dir: ctx.from_prefix_dir.join(&planned.dir),
            })
            .collect();
        let out = fold_row_space(
            &segment_dir,
            &permutation_path,
            &row_entity_path,
            FoldRowSpaceSpec {
                inputs: &inputs,
                identity_key: &ctx.identity_key,
                shard_id: ctx.shard_id,
                scalar_schema: view_schema,
                absent_ok: ctx.absent_ok.get(&view.view).map_or(&[][..], Vec::as_slice),
                tombstones: &plan.tombstones,
                permutation_bound: view.permutation_bound,
            },
        )
        .map_err(|e| failed("pass 1 (row space)", &e))?;

        for name in [
            "morton.u32",
            tessera_store::read::CutIndex::FILE,
            "columns.arrow",
        ] {
            output.push(format!("{segment_rel}/{name}"), segment_dir.join(name));
        }
        for column in &out.presence_columns {
            output.push(
                format!("{segment_rel}/{RENDER_PRESENCE_DIR}/{column}.roaring"),
                tessera_store::render_presence::render_presence_path(&segment_dir, column),
            );
        }
        output.push(permutation_rel, permutation_path);
        output.push(row_entity_rel, row_entity_path);

        segments.push(SegmentDescriptor {
            view: view.view.clone(),
            incarnation: view.incarnation,
            seg_id: ctx.seg_id.clone(),
            row_count: out.row_count,
            entity_lo: 0,
            // Inclusive, and spans the permutation even past the last surviving entity.
            entity_hi: view.permutation_bound.saturating_sub(1),
        });
    }

    Ok(segments)
}

/// Pass 2: the new base postings and `pairs.parquet`. Every ordinal below `dict_len` gets a
/// record, empty or not, so ordinals stay stable across a fold. `pairs.parquet` is rewritten
/// because a carried copy would still list folded deletions.
fn fold_postings(
    plan: &FoldPlan,
    ctx: &FoldContext,
    terms_dir: &Path,
    out: &mut FoldOutput,
) -> Result<PathBuf, MaintenanceFailed> {
    let postings_rel = format!("partitions/{}/terms/postings.arrow", plan.partition);
    let pairs_rel = format!("partitions/{}/terms/pairs.parquet", plan.partition);
    let postings_path = ctx.to_prefix_dir.join(&postings_rel);
    let pairs_path = ctx.to_prefix_dir.join(&pairs_rel);
    let spool_path = terms_dir.join("postings.spool");
    {
        let mut spool =
            PostingsSpool::create(&spool_path).map_err(|e| failed("pass 2 (spool)", &e))?;
        let mut pairs =
            PairsParquetWriter::create(&pairs_path).map_err(|e| failed("pass 2 (pairs)", &e))?;
        let sweep = sweep_term_postings(
            plan.dict_len,
            &ctx.base_postings,
            &ctx.tiers,
            &plan.tombstones,
            plan.small_term_threshold,
            &mut spool,
            |term, entities| {
                pairs
                    .push_iter(term.raw(), entities.iter())
                    .map_err(|e| std::io::Error::other(e.to_string()))
            },
        );
        remove_spool_on_error(
            sweep.and_then(|()| spool.finish(&postings_path)),
            &spool_path,
        )
        .map_err(|e| failed("pass 2 (postings)", &e))?;
        pairs.finish().map_err(|e| failed("pass 2 (pairs)", &e))?;
    }
    out.push(postings_rel, postings_path.clone());
    out.push(pairs_rel, pairs_path);

    Ok(postings_path)
}

/// Pass 2b: one term-image file per view, projecting each new base posting into the view's new row
/// space. It reads pass 2's postings, so folded deletions are already gone. A later flush's rows
/// are an extent and get no images.
fn derive_term_images(
    plan: &FoldPlan,
    ctx: &FoldContext,
    segments: &[SegmentDescriptor],
    postings_path: &Path,
    out: &mut FoldOutput,
) -> Result<Vec<FoldedTermImages>, MaintenanceFailed> {
    let mut term_images: Vec<FoldedTermImages> = Vec::new();
    let postings = PostingsReader::open(postings_path, true)
        .map_err(|e| failed("pass 2b (term images: the new postings)", &e))?;
    let dict_len = postings.term_count();
    let mut index = tessera_store::derived::DerivedIndex::default();
    for segment in segments {
        if segment.row_count == 0 || dict_len == 0 {
            continue;
        }
        let permutation_path = ctx.to_prefix_dir.join(format!(
            "partitions/{}/{}/permutation.bin",
            plan.partition,
            tessera_store::view_rel(&segment.view)
        ));
        // From pass 1's file, so the images match the permutation that is published.
        let permutation = tessera_store::Permutation::load(&permutation_path)
            .map_err(|e| failed("pass 2b (term images: the new permutation)", &e))?;
        let space = tessera_store::RowSpace::new(Arc::new(permutation), segment.row_count);
        let stamp = tessera_store::term_images::TermImageStamp {
            prefix: ctx.to_prefix.clone(),
            view: segment.view.clone(),
            base_seg_id: segment.seg_id.clone(),
            incarnation: segment.incarnation,
            base_rows: segment.row_count,
            bound: space.base().bound(),
        };
        let file = tessera_store::derived::term_image_file(
            &ctx.to_prefix_dir,
            &plan.partition,
            TERM_IMAGE_MANIFEST_N,
            &mut index,
        )
        .map_err(|e| failed("pass 2b (term images: naming the file)", &e))?;

        // `tessera-store` cannot depend on `tessera-authz`, so the postings are adapted here.
        let walk = |term: u32,
                    visit: &mut dyn FnMut(tessera_store::derived::PostingSlice<'_>)|
         -> std::io::Result<()> {
            if let Some(posting) = postings.posting_at(term)? {
                match posting {
                    tessera_authz::PostingRef::Array(bytes) => {
                        visit(tessera_store::derived::PostingSlice::Array(bytes))
                    }
                    tessera_authz::PostingRef::Roaring(bitmap) => {
                        visit(tessera_store::derived::PostingSlice::Roaring(&bitmap))
                    }
                }
            }
            Ok(())
        };
        let summary = tessera_store::term_images::derive_term_images(
            &space,
            dict_len,
            &walk,
            &stamp,
            &file.path,
            tessera_store::term_images::DeriveOptions {
                threads: TERM_IMAGE_THREADS,
            },
        )
        .map_err(|e| failed("pass 2b (term images: the derivation)", &e))?;

        out.push(file.rel.clone(), file.path);
        term_images.push(FoldedTermImages {
            extent: tessera_store::manifest::TermImageExtent {
                path: file.rel,
                view: segment.view.clone(),
                incarnation: segment.incarnation,
                dict_len,
                keep_rows_per_container: tessera_store::term_images::KEEP_ROWS_PER_CONTAINER
                    as u32,
            },
            summary,
        });
    }

    Ok(term_images)
}

/// Pass 3: external-id run 0 and its locator, bounded at the snapshot's entity space so later
/// locator extents stay reachable. Tombstoned entities' keys are dropped, or a lawful re-ingest of
/// that external id would be refused once retirement lifts the deletion.
fn fold_external_ids(
    plan: &FoldPlan,
    ctx: &FoldContext,
    entities_dir: &Path,
    out: &mut FoldOutput,
) -> Result<Option<String>, MaintenanceFailed> {
    let external_id_run = if plan.runs.is_empty() {
        None
    } else {
        let run_paths: Vec<PathBuf> = plan
            .runs
            .iter()
            .map(|rel| ctx.from_prefix_dir.join(rel))
            .collect();
        fold_external_id_runs(
            &run_paths,
            0,
            plan.entity_bound.saturating_sub(1),
            &plan.tombstones,
            entities_dir,
        )
        .map_err(|e| failed("pass 3 (external ids)", &e))?;
        // Run 0 stays first: the sidecar finds the base locator from its directory.
        let run_rel = format!("partitions/{}/entities/external-ids.arrow", plan.partition);
        let locator_rel = format!("partitions/{}/entities/ext-locator.u32", plan.partition);
        out.push(run_rel.clone(), entities_dir.join("external-ids.arrow"));
        out.push(locator_rel, entities_dir.join("ext-locator.u32"));
        Some(run_rel)
    };

    Ok(external_id_run)
}

/// Pass 5: digests each file in `out.written` by reading it back, since the writers cannot hash as
/// they write, and fsyncs them all before `CURRENT` is flipped onto this prefix.
fn digest_and_sync(out: &FoldOutput) -> Result<BTreeMap<String, FileDigest>, MaintenanceFailed> {
    let mut files = BTreeMap::new();
    for (rel, path) in &out.written {
        files.insert(
            rel.clone(),
            crate::flush::digest_of(path)?,
        );
    }
    let paths: Vec<PathBuf> = out.written.iter().map(|(_, path)| path.clone()).collect();
    tessera_store::fsync_written(&paths).map_err(|e| failed("pass 5 (durability)", &e))?;

    Ok(files)
}

/// One past the highest `v#####` under `bundle_root`, from the directory listing, since a
/// discarded fold leaves a complete tree that `CURRENT` never named.
pub(crate) fn next_prefix_name(bundle_root: &Path) -> std::io::Result<String> {
    let mut highest = 0u64;
    for entry in std::fs::read_dir(bundle_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(digits) = name.strip_prefix('v') else {
            continue;
        };
        // `{:05}` is a minimum width, so the hundred-thousandth prefix has six digits.
        if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = digits.parse::<u64>() {
            highest = highest.max(n);
        }
    }
    Ok(format!("v{:05}", highest + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The next prefix steps past an orphaned tree a discarded fold left, as well as the live one.
    #[test]
    fn the_next_prefix_steps_past_every_name_present_including_an_orphan() {
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["v00000", "v00003", "not-a-prefix", "v0004"] {
            std::fs::create_dir(tmp.path().join(name)).unwrap();
        }
        assert_eq!(next_prefix_name(tmp.path()).unwrap(), "v00004");
    }

    /// A six-digit prefix counts, or every later fold would reuse `v100000`.
    #[test]
    fn the_next_prefix_keeps_counting_past_five_digits() {
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["v00000", "v99999", "v100000"] {
            std::fs::create_dir(tmp.path().join(name)).unwrap();
        }
        assert_eq!(next_prefix_name(tmp.path()).unwrap(), "v100001");
    }

    /// An empty bundle root names the first prefix.
    #[test]
    fn the_next_prefix_over_an_empty_root_is_the_first_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(next_prefix_name(tmp.path()).unwrap(), "v00001");
    }
}
