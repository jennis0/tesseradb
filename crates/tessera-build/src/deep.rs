//! `tessera verify --deep` — the structural verifier's deep mode (correctness-suite §11, §12.4).
//!
//! [`verify_deep`] runs the whole of [`crate::verify`] — the read protocol, bijectivity over the
//! full row space, the identity column — and then one linear pass over the structures identity
//! says nothing about: postings, the external-id sidecar, the dictionary extents and the oracle's
//! `pairs.parquet`. It needs no fixture, no generator and no oracle, which is what lets it run
//! against a bundle whose data came from somewhere real.
//!
//! ## What the open already refuses, and why this module does not re-check it
//!
//! Two of §11's checks are discharged by the open the shallow pass performs, fail-closed and
//! unconditionally: Morton codes not non-decreasing refuse at `MortonSlice::load`, and a column
//! shorter than its segment's declared `row_count` refuses at segment load (Arrow itself refuses
//! a batch whose columns disagree in length, and the loader compares the batch against the
//! manifest). Re-running those loops here would be code no input can reach — the same reasoning
//! §11 records for the rejected residual-in-cell check, which cannot fail because the residual is
//! the low half of the same 32-bit split whose high half is the cell code. What holds the claim
//! instead of a comment is the deliberate-damage pair in `tests/verify_deep.rs` (§18 obligation
//! 9): if the loader's refusals are ever relaxed, those tests fail, and the checks move here.
//!
//! ## The external-id agreement is newest-binding-first, NOT a bijection
//!
//! Delete plus re-ingest re-binds an external id (decision 0047): the newest run holding a key
//! carries the live binding, and the superseded row — a forgotten, deleted holder — is retained
//! until the compaction fold executes the deletion. A bijection check (each key exactly one row)
//! would therefore refuse every bundle that has taken a re-ingest. What *is* invariant, in every
//! state every producer leaves (build, flush, coalesce, fold), is row↔slot exactness in both
//! directions: a locator slot addresses a run row bound to exactly that entity, and every run
//! row is addressed back by its own entity's one covering slot. A key appearing in several runs
//! is the accepted 0047 state, not a defect.
//!
//! The sidecar family is also the one exemption from the open-time digest sweep (contracts §0.3
//! deviation 9 — verified lazily, at first touch), so this pass digests each run and locator
//! against the manifest before reading it: a deep verify of a bundle at rest must not leave the
//! only undigested family undigested.
//!
//! ## The pairs check is scoped to the base, deliberately
//!
//! `terms/pairs.parquet` is written by the build and rewritten by the fold, and by nothing else —
//! a flush's delta tiers hold pairs it has never seen. The check is therefore exact equality with
//! the union of the **base** `terms/postings.arrow`, deltas excluded; an unscoped check against
//! base ∪ deltas would refuse every bundle that has absorbed a write.
//!
//! ## Cost model, and what this pass must not be pointed at
//!
//! One linear pass, and it re-hashes the sidecar family, streaming each file rather than reading
//! it whole. Every structure the two agreement directions cross is read through a mapping: the
//! locators from the bundle, the runs' entity columns from one scratch file this pass writes and
//! unlinks. What it holds is a row count and a name per run, and one span per flush extent. The
//! base locator is four bytes for every entity in the corpus and a run carries the corpus's own
//! ids, neither of which a verifier can put in the heap. Cadence is the suite's concern (§11:
//! per-stage at the small tiers, per-run above).
//!
//! ⊘ **At rest only, for now.** This resolves `CURRENT` through `open_bundle`, so a fold flipping
//! `CURRENT` mid-pass could swap the prefix under it. §12.4's answer — name the prefix once and
//! never re-read `CURRENT`, with a vanishing file reported as a race rather than a defect — needs
//! `tessera-store`'s named-prefix open made public, which has not happened. Until it has, point
//! this at a bundle no live engine is publishing into.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use arrow::array::{Array, BinaryArray, UInt32Array, UInt64Array};
use arrow::ipc::reader::FileReader as ArrowFileReader;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use sha2::{Digest, Sha256};

use tessera_authz::{DeltaTier, PostingRef, PostingsReader};
use tessera_store::manifest::{FileDigest, Manifest, SegmentsManifest};

use crate::error::{BuildError, Result};
use crate::VerifyReport;

/// Options for [`verify_deep`].
#[derive(Debug, Clone)]
pub struct VerifyOpts {
    /// The points file this bundle claims to have been built from (correctness-suite §11.1).
    ///
    /// ⊘ **Specified, not implemented.** Checking it needs `source.digest` in `MANIFEST.json`,
    /// and that field belongs to contracts §2.2 — an amendment that has not been made. Passing
    /// `Some` is refused rather than silently ignored, so no harness can believe a binding was
    /// checked when nothing exists to check it against.
    pub source: Option<PathBuf>,
    /// The identity check's window threshold, in rows ([`crate::verify_with_window_rows`]).
    ///
    /// [`Default`] is [`crate::DIRECT_WINDOW_ROWS`], which is what an operator's `verify --deep`
    /// runs. A test lowers it to reach the partition route with a fixture it can afford to build.
    pub direct_window_rows: u64,
}

impl Default for VerifyOpts {
    fn default() -> VerifyOpts {
        VerifyOpts {
            source: None,
            direct_window_rows: crate::DIRECT_WINDOW_ROWS,
        }
    }
}

/// What [`verify_deep`] checked, beyond the shallow pass.
#[derive(Debug, Clone)]
pub struct VerifyDeepReport {
    pub shallow: VerifyReport,
    /// Base postings records walked (sorted, duplicate-free, bounded), across partitions.
    pub terms: u64,
    /// Delta postings tiers walked under the same rules.
    pub delta_tiers: usize,
    /// `pairs.parquet` rows matched one-to-one against the base postings' union; 0 where the
    /// bundle legitimately carries no pairs file (`--no-oracle-pairs`).
    pub pairs_rows: u64,
    /// Dictionary records parsed across the extent lists, none repeated (decision 0042).
    pub dict_records: u64,
    /// External-id run rows confirmed to agree with the locator side in both directions; 0 where
    /// the deployment minted no external ids — the ordinary case, not a degraded one.
    pub external_id_bindings: u64,
    /// Record-blob rows walked across the base blob and every extent of every partition, each
    /// one's identity checked against the block that holds it and against the has-row bitmap's
    /// member at its rank ([`check_record_blobs`]); 0 where no column is blob-resident.
    pub record_rows: u64,
    /// `(segment, column)` pairs confirmed to hold a group-scoped **render** family's lane
    /// ([`check_scoped_render_lanes`]); 0 where no family declares `render`, which is every
    /// bundle whose attributes are entity-scoped.
    pub scoped_render_lanes: u64,
    /// Leaf Morton cells confirmed to begin where `cuts.u32` says and to hold ascending
    /// identities ([`check_cut_index`]), across every segment of every view.
    pub cells: u64,
}

/// Deep-verify the bundle at `root`: the shallow [`crate::verify`] pass, then §11's structural
/// checks over postings, dictionary extents, the external-id sidecar and `pairs.parquet`. See the
/// module doc for what each check accepts on purpose.
pub fn verify_deep(root: &Path, opts: &VerifyOpts) -> Result<VerifyDeepReport> {
    if opts.source.is_some() {
        return Err(BuildError::Invalid(
            "verifying against a source file is specified (correctness-suite §11.1) but not \
             implemented: it needs `source.digest` in MANIFEST.json, a contracts §2.2 amendment \
             that has not been made. Refused rather than ignored, so nothing can believe a \
             binding was checked"
                .into(),
        ));
    }

    let (bundle, shallow) = crate::verified_open(root, opts.direct_window_rows)?;
    let prefix_dir = root.join(&shallow.prefix);

    let mut report = VerifyDeepReport {
        shallow,
        terms: 0,
        delta_tiers: 0,
        pairs_rows: 0,
        dict_records: 0,
        external_id_bindings: 0,
        record_rows: 0,
        scoped_render_lanes: 0,
        cells: 0,
    };

    // Sorted so two runs over the same defective bundle refuse with the same message.
    let mut partitions: Vec<_> = bundle.partitions.iter().collect();
    partitions.sort_by(|a, b| a.0.cmp(b.0));
    for (phash, partition) in partitions {
        check_postings_and_pairs(
            &prefix_dir,
            phash,
            &bundle.manifest,
            &partition.manifest,
            &mut report,
        )?;
        check_dict_extents(&prefix_dir, &partition.manifest, &mut report)?;
        check_external_ids(
            root,
            &prefix_dir,
            &bundle.manifest,
            &partition.manifest,
            &mut report,
        )?;
        check_record_blobs(
            &prefix_dir,
            phash,
            &bundle.manifest,
            &partition.manifest,
            &mut report,
        )?;
        check_scoped_render_lanes(&bundle.manifest, phash, partition, &mut report)?;
        check_cut_index(phash, partition, &mut report)?;
    }

    Ok(report)
}

/// **`cuts.u32` names exactly the rows at which a leaf Morton cell begins, and each cell's
/// identities ascend** (contracts §2.6).
///
/// The index is what selection walks instead of reading every visible row of a tile, and both
/// halves of that walk rest on a property no other check establishes. The boundaries being where
/// the code changes is what makes the cells cover the segment without overlapping — a boundary too
/// few merges two cells and the ascending-identity property fails across the join; one too many
/// splits a cell, which loses no row but ends a prefix early and would serve a smaller `C_θ`.
/// Identities ascending within a cell is the row order itself (`(morton, tessera_id)`), stated
/// where selection depends on it rather than assumed from the producer.
///
/// The open has already refused a `cuts.u32` that is not strictly ascending, that starts anywhere
/// but row 0, or whose last cell begins past the segment
/// ([`tessera_store::read::CutIndex::load`]); what needs both columns is checked here.
///
/// One forward pass over `morton.u32` and the identity column, holding a row index and the
/// previous row's two values.
fn check_cut_index(
    phash: &str,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let mut views: Vec<_> = partition.views.iter().collect();
    views.sort_by(|a, b| a.0.cmp(b.0));
    for (view, data) in views {
        for segment in &data.segments {
            let codes = segment.morton.u32();
            let starts = segment.cuts.starts();
            let ids = segment.columns.tessera_id();
            let where_at = |row: usize| {
                format!("partition {phash}, view '{view}', segment '{}', row {row}", segment.seg_id)
            };
            let mut cell = 0usize;
            for row in 0..codes.len() {
                let opens = row == 0 || codes[row] != codes[row - 1];
                if opens {
                    match starts.get(cell) {
                        Some(&start) if start as usize == row => cell += 1,
                        Some(&start) => {
                            return Err(BuildError::Invalid(format!(
                                "cuts.u32 names row {start} where the Morton code changes at \
                                 row {row} ({})",
                                where_at(row)
                            )))
                        }
                        None => {
                            return Err(BuildError::Invalid(format!(
                                "cuts.u32 names {} cells and the Morton code changes again at \
                                 row {row} ({})",
                                starts.len(),
                                where_at(row)
                            )))
                        }
                    }
                } else {
                    if starts.get(cell).is_some_and(|&start| start as usize == row) {
                        return Err(BuildError::Invalid(format!(
                            "cuts.u32 opens a cell at row {row}, where the Morton code is \
                             unchanged ({})",
                            where_at(row)
                        )));
                    }
                    if ids[row] <= ids[row - 1] {
                        return Err(BuildError::Invalid(format!(
                            "identity {} does not follow {} inside one Morton cell — selection \
                             reads a cell's identities as ascending ({})",
                            ids[row],
                            ids[row - 1],
                            where_at(row)
                        )));
                    }
                }
            }
            if cell != starts.len() {
                return Err(BuildError::Invalid(format!(
                    "cuts.u32 names {} cells and the Morton column holds {cell} (partition \
                     {phash}, view '{view}', segment '{}')",
                    starts.len(),
                    segment.seg_id
                )));
            }
            report.cells += cell as u64;
        }
    }
    Ok(())
}

/// **A rendered group-scoped family's lane is present in every build segment of every view that
/// renders it** (`views.md` §5).
///
/// The lane is what `render` on a family *is*: one column in the row tail of each view of the
/// group, and of any group sharing those views. It is the one artefact of the placement whose
/// absence is silent — serving reads a missing scoped column as the ordinary absence a flush
/// leaves, so a build that wrote no lane at all is indistinguishable, at the wire, from one whose
/// rows genuinely carry no value. Nothing else in this pass would notice: the file digests match
/// (the lane changes `columns.arrow`, which is digested as a whole), the row space is a bijection
/// either way, and the identity column is untouched.
///
/// **Every segment of a view of the family's own group is checked, the build's and the write
/// path's alike.** A flush of such a view writes the lane from the row's own scoped values, and a
/// merge and a fold take the view's schema rather than the bundle's — so a missing lane there is
/// the same silent defect it is at a build, not the write half's deliberate absence it once was.
///
/// ⊘ **A view of a group that declares `members` keeps the build-only exemption.** Its rows render
/// the owner's family, but no batch into it may carry a value: the column is the owner's, and a
/// second writer for one `(entity, view)` column is two layers claiming one entity (`views.md`
/// §5's ingest paragraph). A flush of such a view writes the lane holding absences once the family
/// lists the key — and cannot before, the owner's own first flush being what puts it there — so a
/// segment of a sharing group's view may legitimately hold no lane and requiring one would refuse
/// a bundle that has ingested in that order. A build segment is one whose `columns.arrow` is named
/// in `MANIFEST.files`, which is contracts §2.2's own division: that map covers what existed at
/// build time and `SEGMENTS-<n>.files` covers what has appeared since.
fn check_scoped_render_lanes(
    manifest: &tessera_store::manifest::Manifest,
    phash: &str,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let families: Vec<_> = manifest
        .scoped_scalars()
        .into_iter()
        .filter(|f| f.render)
        .collect();
    if families.is_empty() {
        return Ok(());
    }
    for (view_id, view) in &partition.views {
        // Which families this view renders — its own group's, or those of the group it declares
        // `members` of, the keys being the owner's (§3.3). Stated here off the manifest's roster,
        // as the engine states it off the same roster at the request and the build off its
        // arguments; all three must agree, and this is the one that can catch a disagreement.
        let Some((group, key)) = manifest.groups.iter().find_map(|g| {
            let key = view_id
                .strip_prefix(g.name.as_str())?
                .strip_prefix(tessera_store::GROUP_SEPARATOR)?;
            g.views
                .iter()
                .any(|v| v.key == key)
                .then_some((g.name.as_str(), key))
        }) else {
            continue;
        };
        let owner = manifest
            .groups
            .iter()
            .find(|g| g.name == group)
            .and_then(|g| g.members_of.as_deref())
            .unwrap_or(group);
        let owed: Vec<_> = families
            .iter()
            .filter(|f| {
                f.group == owner
                    && f.views
                        .contains(&format!("{owner}{}{key}", tessera_store::GROUP_SEPARATOR))
            })
            .collect();
        if owed.is_empty() {
            continue;
        }
        for segment in &view.segments {
            // The `files` map's own key: partition-qualified, forward slashes, exactly as the
            // build laid it down (contracts §2.2).
            let rel = format!(
                "partitions/{phash}/{}/segments/{}/columns.arrow",
                tessera_store::view_rel(view_id),
                segment.seg_id
            );
            if owner != group && !manifest.files.contains_key(&rel) {
                continue;
            }
            for family in &owed {
                if segment.columns.scalar(&family.name).is_none() {
                    return Err(BuildError::Invalid(format!(
                        "view '{view_id}', segment '{}': the manifest declares '{}' as a \
                         `render` family of group '{}' with a column for this view, and the \
                         segment's tail does not hold it. A rendered scoped column is served from \
                         the row tail, and its absence is read as an absent value rather than as \
                         a fault (views §5)",
                        segment.seg_id, family.name, family.group
                    )));
                }
                report.scoped_render_lanes += 1;
            }
        }
    }
    Ok(())
}

/// Join a manifest-supplied, forward-slash relative path onto the prefix directory, refusing
/// anything that could escape it — the same rule the read protocol applies, restated here because
/// its implementation is private to `tessera-store`.
fn join_rel(prefix_dir: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        return Err(BuildError::Invalid(format!(
            "manifest path '{rel}' is not a safe prefix-relative path"
        )));
    }
    let mut path = prefix_dir.to_path_buf();
    for component in rel.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(BuildError::Invalid(format!(
                "manifest path '{rel}' contains an unsafe component"
            )));
        }
        path.push(component);
    }
    Ok(path)
}

/// Postings sorted, duplicate-free and bounded by `entity_id_high_water` — the base file and
/// every delta tier — and `pairs.parquet` exactly the union of the **base** postings (see the
/// module doc for why the deltas are excluded on purpose).
fn check_postings_and_pairs(
    prefix_dir: &Path,
    phash: &str,
    bundle_manifest: &Manifest,
    partition_manifest: &SegmentsManifest,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let high_water = partition_manifest.entity_id_high_water;
    let named = |rel: &str| {
        partition_manifest.files.contains_key(rel) || bundle_manifest.files.contains_key(rel)
    };

    let postings_rel = format!("partitions/{phash}/terms/postings.arrow");
    let pairs_rel = format!("partitions/{phash}/terms/pairs.parquet");
    if named(&postings_rel) {
        let path = join_rel(prefix_dir, &postings_rel)?;
        let reader =
            PostingsReader::open(&path, false).map_err(|e| BuildError::io(&path, e))?;

        let mut pairs = if named(&pairs_rel) {
            Some(PairsCursor::open(&join_rel(prefix_dir, &pairs_rel)?)?)
        } else {
            None
        };

        for t in 0..reader.term_count() {
            let posting = reader
                .posting_at(t)
                .map_err(|e| BuildError::io(&path, e))?
                .expect("t < term_count, so the record exists");
            let what = format!("{postings_rel}: term {t}");
            walk_posting(&what, &posting, high_water, |entity| {
                let Some(cursor) = pairs.as_mut() else {
                    return Ok(());
                };
                match cursor.next()? {
                    Some((pe, pt)) if pt == t && pe == entity as u64 => Ok(()),
                    Some((pe, pt)) => Err(BuildError::Invalid(format!(
                        "{pairs_rel}: holds (entity {pe}, term {pt}) where the base postings \
                         hold (entity {entity}, term {t}) — the file must be exactly the union \
                         of the base postings it was written with"
                    ))),
                    None => Err(BuildError::Invalid(format!(
                        "{pairs_rel}: ends before the base postings do — (entity {entity}, \
                         term {t}) is missing from it"
                    ))),
                }
            })?;
            report.terms += 1;
        }
        if let Some(mut cursor) = pairs {
            if let Some((pe, pt)) = cursor.next()? {
                return Err(BuildError::Invalid(format!(
                    "{pairs_rel}: holds (entity {pe}, term {pt}) beyond the base postings' \
                     union — the file is written by the build and rewritten by the fold, and \
                     must carry no pair the base it was written with does not"
                )));
            }
            report.pairs_rows += cursor.consumed;
        }
    } else if named(&pairs_rel) {
        return Err(BuildError::Invalid(format!(
            "{pairs_rel} is named but {postings_rel} is not — there is no base to scope its \
             union against"
        )));
    }

    for delta_rel in &partition_manifest.deltas {
        let path = join_rel(prefix_dir, delta_rel)?;
        let tier = DeltaTier::open(&path).map_err(|e| BuildError::io(&path, e))?;
        for ordinal in tier.ordinals() {
            let posting = tier
                .posting_at(ordinal)
                .map_err(|e| BuildError::io(&path, e))?
                .expect("the ordinal came from the tier's own iterator");
            let what = format!("{delta_rel}: term ordinal {ordinal}");
            walk_posting(&what, &posting, high_water, |_| Ok(()))?;
        }
        report.delta_tiers += 1;
    }
    Ok(())
}

/// Walk one posting's entities in stored order, refusing an unsorted or duplicated entry and any
/// entity at or past `high_water`, handing each accepted entity to `and_then` (the pairs
/// comparison, where one is running). A tag-1 record is a Roaring bitmap — a set, sorted and
/// duplicate-free by construction — so for it the order check is vacuous and the walk exists for
/// the bound and for `and_then`.
fn walk_posting(
    what: &str,
    posting: &PostingRef<'_>,
    high_water: u64,
    mut and_then: impl FnMut(u32) -> Result<()>,
) -> Result<()> {
    let mut previous: Option<u32> = None;
    let mut visit = |entity: u32| -> Result<()> {
        if let Some(previous) = previous {
            if entity <= previous {
                return Err(BuildError::Invalid(format!(
                    "{what}: entity {entity} follows {previous} — postings must be sorted \
                     strictly ascending, duplicate-free"
                )));
            }
        }
        if entity as u64 >= high_water {
            return Err(BuildError::Invalid(format!(
                "{what}: entity {entity} is at or past entity_id_high_water {high_water}"
            )));
        }
        previous = Some(entity);
        and_then(entity)
    };
    match posting {
        PostingRef::Array(bytes) => {
            // A payload length that is not a multiple of 4 was refused at the reader's open.
            for chunk in bytes.as_chunks::<4>().0 {
                visit(u32::from_le_bytes(*chunk))?;
            }
        }
        PostingRef::Roaring(view) => {
            for entity in view.iter() {
                visit(entity)?;
            }
        }
    }
    Ok(())
}

/// A sequential cursor over `pairs.parquet`'s `(entity_id, term_id)` rows, in file order — which
/// the writer fixes as `(term_id, entity_id)`-sorted, exactly the order the base postings walk
/// produces.
struct PairsCursor {
    path: PathBuf,
    reader: ParquetRecordBatchReader,
    batch: Option<(UInt64Array, UInt32Array)>,
    idx: usize,
    consumed: u64,
}

impl PairsCursor {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .and_then(|builder| builder.build())
            .map_err(|e| BuildError::Parquet {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?;
        Ok(PairsCursor {
            path: path.to_path_buf(),
            reader,
            batch: None,
            idx: 0,
            consumed: 0,
        })
    }

    fn next(&mut self) -> Result<Option<(u64, u32)>> {
        loop {
            if let Some((entities, terms)) = &self.batch {
                if self.idx < entities.len() {
                    let row = (entities.value(self.idx), terms.value(self.idx));
                    self.idx += 1;
                    self.consumed += 1;
                    return Ok(Some(row));
                }
            }
            let Some(batch) = self.reader.next() else {
                return Ok(None);
            };
            let batch = batch.map_err(|e| BuildError::Arrow {
                path: self.path.clone(),
                detail: e.to_string(),
            })?;
            let malformed = |detail: &str| {
                BuildError::Invalid(format!("{}: {detail}", self.path.display()))
            };
            let entities = batch
                .column_by_name("entity_id")
                .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
                .ok_or_else(|| malformed("no uint64 'entity_id' column"))?
                .clone();
            let terms = batch
                .column_by_name("term_id")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
                .ok_or_else(|| malformed("no uint32 'term_id' column"))?
                .clone();
            self.batch = Some((entities, terms));
            self.idx = 0;
        }
    }
}

/// The record blob's addressing, walked layer by layer: every block decompressed, every row
/// framed inside its extent, every row's identity taken from the block that holds it and compared
/// with the has-row bitmap's member at its rank (`records-and-search.md` §3, §10).
///
/// **This is the one structure whose addressing nothing else in the bundle can see.** The other
/// homes are positional, so there is no offset to be wrong; the blob reaches a row through three
/// derived indirections, and a fold or coalesce that rewrote them wrongly would produce a bundle
/// whose files all match their digests and whose drill-down serves a neighbour's record. The read
/// path refuses that per request; without this pass nothing reports it offline, so a defect would
/// first be seen by a viewer receiving someone else's record.
///
/// The walk is [`tessera_filter::RecordBlob::for_each_row`], which is the reader's own self-check
/// and not a transcription of it: a verifier with its own idea of the format would agree with a
/// blob the reader refuses, or refuse one it serves.
///
/// **The base blob is walked where the manifest names its files**, not where the directory happens
/// to exist: a build writes `attrs/record/` only where a column is blob-resident, and probing the
/// filesystem would read a base deleted from under the manifest as "this bundle has no base".
/// Extents come from the segments manifest's two lists, the flushes' and the artifact levels',
/// which hold the same format and take the same reader.
fn check_record_blobs(
    prefix_dir: &Path,
    phash: &str,
    manifest: &Manifest,
    partition_manifest: &SegmentsManifest,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let base_rel = format!(
        "partitions/{phash}/attrs/record/{}",
        tessera_filter::RECORD_BLOCKS_FILE
    );
    let mut layers: Vec<(String, PathBuf, PathBuf, PathBuf)> = Vec::new();
    if manifest.files.contains_key(&base_rel) {
        let dir = prefix_dir
            .join("partitions")
            .join(phash)
            .join("attrs")
            .join("record");
        layers.push((
            format!("partitions/{phash}/attrs/record"),
            dir.join(tessera_filter::RECORD_BLOCKS_FILE),
            dir.join(tessera_filter::RECORD_HASROW_FILE),
            dir.join(tessera_filter::RECORD_DIRECTORY_FILE),
        ));
    }
    for extent in partition_manifest
        .record_extents
        .iter()
        .chain(partition_manifest.artifact_record_extents.iter())
    {
        layers.push((
            extent.blocks.clone(),
            join_rel(prefix_dir, &extent.blocks)?,
            join_rel(prefix_dir, &extent.hasrow)?,
            join_rel(prefix_dir, &extent.directory)?,
        ));
    }

    for (name, blocks, hasrow, directory) in layers {
        // Mapped, not read: `Access::Read` pulls `blocks.bin` and `directory.arrow` into the
        // heap whole, and a corpus's record blocks are the corpus. The open-time digest sweep
        // has already vouched for both files, so nothing here needs a private copy of them.
        //
        // Sequential because the walk visits each block exactly once and wants the drop-behind:
        // without it a deep verify on a serving box leaves the whole blob resident in the cache
        // the request path is using. `RecordBlob::open` takes one access for both files, so the
        // hint reaches `directory.arrow` as well — it is decoded at open and then read in block
        // order, so the most a drop-behind costs there is one re-fault of a file whose size is a
        // handful of bytes per block.
        let blob = tessera_filter::RecordBlob::open(
            &blocks,
            &hasrow,
            &directory,
            tessera_filter::Access::MappedSequential,
        )
        .map_err(|e| BuildError::Invalid(format!("{name}: {e}")))?;
        let mut rows = 0u64;
        blob.for_each_row(&mut |_, _| {
            rows += 1;
            Ok(())
        })
        .map_err(|e| BuildError::Invalid(format!("{name}: {e}")))?;
        if rows != blob.rows() {
            return Err(BuildError::Invalid(format!(
                "{name}: the walk returned {rows} rows where the has-row bitmap holds {} — the \
                 blocks and the bitmap do not address the same rows",
                blob.rows()
            )));
        }
        report.record_rows += rows;
    }
    Ok(())
}

/// Dictionary extents positional and never repeating a descriptor (decision 0042): each extent's
/// record count must equal what the manifest declares — a term's ordinal is its position in the
/// concatenation, so a miscount shifts every later term — and no descriptor may appear twice
/// across the list, which after a restart renumbers everything past the repeat. The reader
/// (`Dict::load`) tolerates a repeat by skipping it, deliberately; this is the artefact-side
/// statement that no correct writer produces one.
///
/// **This is the pass's remaining unbounded arm.** It reads each extent whole and holds a
/// `HashSet` of every descriptor it has seen, so its memory is the term vocabulary's — not the
/// corpus's, which is why it survived the bounding of the row space, the record blob and the
/// external-id sidecar, but a deployment whose vocabulary is itself corpus-scale would find it
/// here. Bounding it needs the descriptors sorted rather than hashed, which is a partition pass
/// like the identity check's.
fn check_dict_extents(
    prefix_dir: &Path,
    partition_manifest: &SegmentsManifest,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for extent in &partition_manifest.dict_extents {
        let path = join_rel(prefix_dir, &extent.path)?;
        let bytes = fs::read(&path).map_err(|e| BuildError::io(&path, e))?;
        let mut offset = 0usize;
        let mut records = 0u64;
        while offset < bytes.len() {
            let truncated = |what: &str| {
                BuildError::Invalid(format!(
                    "{}: record {records} {what} — the extent is not a whole number of \
                     `u32 length ‖ descriptor` records",
                    extent.path
                ))
            };
            if bytes.len() - offset < 4 {
                return Err(truncated("cuts off inside its length field"));
            }
            let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
                as usize;
            offset += 4;
            if bytes.len() - offset < len {
                return Err(truncated("overruns the end of the file"));
            }
            let descriptor = &bytes[offset..offset + len];
            if !seen.insert(descriptor.to_vec()) {
                return Err(BuildError::Invalid(format!(
                    "{}: record {records} repeats a descriptor an earlier extent record already \
                     carries — ordinals are positions in the concatenation, and a repeat \
                     renumbers every term after it across a restart (decision 0042)",
                    extent.path
                )));
            }
            offset += len;
            records += 1;
        }
        if records != extent.records {
            return Err(BuildError::Invalid(format!(
                "{}: holds {records} records but the manifest declares {} — the extent list is \
                 positional, so the declared counts are what place every later extent's terms",
                extent.path, extent.records
            )));
        }
        report.dict_records += records;
    }
    Ok(())
}

/// The locator sentinel for "this entity has no caller-supplied external id" (contracts §2.4
/// r6) — the ordinary case, never a missing value.
const LOCATOR_NONE: u32 = 0xFFFF_FFFF;

/// One external-id run: its name, how many rows it holds, and where those rows begin in the
/// concatenation a base-locator ordinal addresses.
struct RunRows {
    rel: String,
    rows: u64,
    offset: u64,
}

/// One locator, mapped: the base file over `[0, len)`, or a flush extent over its own span, with
/// the run its ordinals index.
struct ExtentSlots {
    rel: String,
    entity_lo: u64,
    entity_hi: u64,
    run: usize,
    slots: Slots,
}

/// One file of the sidecar family, mapped.
///
/// **The bytes are checked through the mapping the pass then reads from.** This family is the one
/// exemption from the open-time digest sweep (contracts §0.3 deviation 9), so a deep verify is the
/// only place its bytes are checked at all; hashing a file, closing it and opening it again would
/// leave a same-length rewrite between the two used unverified. There is one mapping, the digest is
/// taken over it, and every read below comes out of it.
struct Mapped {
    map: Option<memmap2::Mmap>,
    len: u64,
}

impl Mapped {
    /// Map `path`. A zero-length file maps to nothing, which `memmap2` refuses and which has no
    /// bytes to read anyway.
    fn open(path: &Path) -> Result<Mapped> {
        let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
        let len = file.metadata().map_err(|e| BuildError::io(path, e))?.len();
        if len == 0 {
            return Ok(Mapped { map: None, len: 0 });
        }
        // SAFETY: the mapping is read-only and no reference into it outlives it. A bundle file
        // rewritten under a running verify is the race §12.4 records for the whole pass; this
        // mapping neither adds to it nor is shared with another process.
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| BuildError::io(path, e))?;
        Ok(Mapped {
            map: Some(map),
            len,
        })
    }

    fn bytes(&self) -> &[u8] {
        self.map.as_ref().map(|map| &map[..]).unwrap_or(&[])
    }
}

/// A little-endian `u32` array over a mapping: a locator from the bundle, or the concatenated run
/// entity columns this pass writes beside it.
///
/// Both are addressed at an index the *other* side hands over — a locator at an entity a run row
/// names, a run row at an ordinal a locator slot names — so neither can be streamed, and both are
/// four bytes for every entity or every binding in the corpus. A mapping gives the random access
/// without the heap.
struct Slots {
    bytes: Mapped,
    len: usize,
}

impl Slots {
    /// Read `bytes` as exactly `declared_len` slots.
    fn over(rel: &str, bytes: Mapped, declared_len: u64) -> Result<Slots> {
        let expected = declared_len.saturating_mul(4);
        if bytes.len != expected {
            return Err(BuildError::Invalid(format!(
                "{rel}: {} bytes where the manifest-implied length is {declared_len} u32 slots \
                 ({expected} bytes)",
                bytes.len
            )));
        }
        Ok(Slots {
            bytes,
            len: declared_len as usize,
        })
    }

    fn len(&self) -> usize {
        self.len
    }

    /// Slot `i`. Four bytes read and assembled rather than a cast: the mapping's alignment is the
    /// page's, but a file's contents are not this code's to make claims about.
    fn get(&self, i: usize) -> u32 {
        let at = i * 4;
        u32::from_le_bytes(
            self.bytes.bytes()[at..at + 4]
                .try_into()
                .expect("four bytes"),
        )
    }
}

/// The external-id locator and its sidecar agreeing in both directions, newest binding first —
/// see the module doc for the exact invariant and for why a repeated key across runs is the
/// accepted state rather than a defect.
fn check_external_ids(
    root: &Path,
    prefix_dir: &Path,
    bundle_manifest: &Manifest,
    partition_manifest: &SegmentsManifest,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let runs_rel = &partition_manifest.external_id_runs;
    if runs_rel.is_empty() {
        // No sidecar at all: the ordinary state for a deployment whose callers supplied no
        // external ids (contracts §2.4 r6), not a degraded one.
        return Ok(());
    }
    let high_water = partition_manifest.entity_id_high_water;

    // Map each file and digest it against the manifest through that mapping, which is what the
    // reads below then come out of ([`Mapped`]). Nothing is read into the heap: the base locator
    // is four bytes an entity.
    let verified_map = |rel: &str| -> Result<Mapped> {
        let digest: &FileDigest = partition_manifest
            .files
            .get(rel)
            .or_else(|| bundle_manifest.files.get(rel))
            .ok_or_else(|| {
                BuildError::Invalid(format!(
                    "{rel}: named by the sidecar lists but digested by neither files map"
                ))
            })?;
        let path = join_rel(prefix_dir, rel)?;
        let mapped = Mapped::open(&path)?;
        if mapped.len != digest.size
            || crate::hex_digest(Sha256::digest(mapped.bytes()).as_slice()) != digest.sha256
        {
            return Err(BuildError::Invalid(format!(
                "{rel}: bytes do not match the manifest digest — this family is exempt from the \
                 open-time sweep (contracts §0.3 deviation 9), so the deep pass is where a \
                 corrupt sidecar file is caught at rest"
            )));
        }
        Ok(mapped)
    };

    // One pass over the runs: each one's keys ascend, each one's entities are below the high
    // water, and the entity column is copied out to a single scratch file in listed order. That
    // file is what a base-locator ordinal addresses, and what direction two reads back in run
    // order.
    let scratch = crate::VerifyTmp::create(root)?;
    let bound_path = scratch.path().join("ext-run-entities.u32");
    let mut runs: Vec<RunRows> = Vec::with_capacity(runs_rel.len());
    let mut total = 0u64;
    {
        let file = File::create(&bound_path).map_err(|e| BuildError::io(&bound_path, e))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        for rel in runs_rel {
            let rows = scan_run(
                rel,
                verified_map(rel)?.bytes(),
                high_water,
                &mut out,
                &bound_path,
            )?;
            runs.push(RunRows {
                rel: rel.clone(),
                rows,
                offset: total,
            });
            total += rows;
        }
        out.flush().map_err(|e| BuildError::io(&bound_path, e))?;
    }
    let bound = Slots::over("the run entity columns", Mapped::open(&bound_path)?, total)?;

    // The base locator lives beside the first run under the same derivation the sidecar uses.
    let locator_rel = match runs_rel[0].rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/ext-locator.u32"),
        None => "ext-locator.u32".to_string(),
    };
    let base_len = bundle_manifest.entity_id_high_water;
    let base = Slots::over(&locator_rel, verified_map(&locator_rel)?, base_len)?;

    let mut extents: Vec<ExtentSlots> =
        Vec::with_capacity(partition_manifest.locator_extents.len());
    for extent in &partition_manifest.locator_extents {
        if extent.entity_hi < extent.entity_lo {
            return Err(BuildError::Invalid(format!(
                "{}: entity span {}..={} is inverted",
                extent.path, extent.entity_lo, extent.entity_hi
            )));
        }
        let run = runs_rel
            .iter()
            .position(|rel| rel == &extent.external_id_run)
            .ok_or_else(|| {
                BuildError::Invalid(format!(
                    "{}: indexes run '{}', which the manifest does not list — its ordinals \
                     cannot be resolved",
                    extent.path, extent.external_id_run
                ))
            })?;
        let span = extent.entity_hi - extent.entity_lo + 1;
        let slots = Slots::over(&extent.path, verified_map(&extent.path)?, span)?;
        extents.push(ExtentSlots {
            rel: extent.path.clone(),
            entity_lo: extent.entity_lo,
            entity_hi: extent.entity_hi,
            run,
            slots,
        });
    }

    // Direction one: every locator slot addresses a row bound to exactly its entity.
    for entity in 0..base.len() {
        let slot = base.get(entity);
        if slot == LOCATOR_NONE {
            continue;
        }
        let global = slot as u64;
        if global >= total {
            return Err(BuildError::Invalid(format!(
                "{locator_rel}: entity {entity}'s ordinal {global} is past the {total} \
                 concatenated run rows"
            )));
        }
        let run = runs.partition_point(|run| run.offset <= global) - 1;
        let claims = bound.get(global as usize) as u64;
        if claims != entity as u64 {
            return Err(BuildError::Invalid(format!(
                "{locator_rel}: entity {entity}'s slot addresses a row of '{}' bound to entity \
                 {claims} — the locator and the runs disagree",
                runs[run].rel
            )));
        }
    }
    for extent in &extents {
        for i in 0..extent.slots.len() {
            let slot = extent.slots.get(i);
            if slot == LOCATOR_NONE {
                continue;
            }
            let entity = extent.entity_lo + i as u64;
            let run = &runs[extent.run];
            if slot as u64 >= run.rows {
                return Err(BuildError::Invalid(format!(
                    "{}: entity {entity}'s ordinal {slot} is past the end of run '{}' ({} rows)",
                    extent.rel, run.rel, run.rows
                )));
            }
            let claims = bound.get((run.offset + slot as u64) as usize) as u64;
            if claims != entity {
                return Err(BuildError::Invalid(format!(
                    "{}: entity {entity}'s slot addresses a row of '{}' bound to entity {claims} \
                     — the locator and the runs disagree",
                    extent.rel, run.rel
                )));
            }
        }
    }

    // Direction two: every run row is addressed back by a slot of its own entity, among the
    // locators the sidecar asks for that entity. This is what refuses a dangling binding (a row
    // no locator can reach) and, with direction one, two slots claiming one row. A key bound in
    // several runs passes both directions: each of its rows has its own entity, and each entity a
    // slot in the locator beside that row's run.
    for (r, run) in runs.iter().enumerate() {
        for i in 0..run.rows {
            let entity = bound.get((run.offset + i) as usize) as u64;
            let mut covered = false;
            let agrees = tessera_store::locators_covering(entity, base_len, &extents, |x| {
                (x.entity_lo, x.entity_hi)
            })
            .any(|locator| {
                covered = true;
                match locator {
                    tessera_store::Locator::Base => {
                        let slot = base.get(entity as usize);
                        slot != LOCATOR_NONE && slot as u64 == run.offset + i
                    }
                    tessera_store::Locator::Extent(x) => {
                        let extent = &extents[x];
                        extent.run == r
                            && extent.slots.get((entity - extent.entity_lo) as usize) as u64 == i
                    }
                }
            });
            if !covered {
                return Err(BuildError::Invalid(format!(
                    "{}: row {i} binds entity {entity}, which no locator covers — the reverse \
                     direction could never answer for it",
                    run.rel
                )));
            }
            if !agrees {
                return Err(BuildError::Invalid(format!(
                    "{}: row {i} binds entity {entity}, but the locator side does not point \
                     back at it — the two directions must agree",
                    run.rel
                )));
            }
        }
    }

    report.external_id_bindings += total;
    Ok(())
}

/// Scan one external-id run: `(external_id: Binary, entity_id: UInt32)`, keys strictly ascending
/// within the run (a duplicate inside one run would make a binding unreachable to the sidecar's
/// binary search — across runs is the accepted 0047 state), entities below `high_water`. Each
/// row's entity is appended to `out` as four little-endian bytes; the row count is returned.
///
/// **Only the previous key is held.** The ascent is a comparison between neighbours, and a run's
/// keys are the caller's own ids: holding them all is what made this pass's memory the corpus's.
/// The high-water fault is carried to the end of the run rather than raised where it is found, so
/// a run with both faults still reports the ordering one, which is the first a reader can act on.
fn scan_run(
    rel: &str,
    bytes: &[u8],
    high_water: u64,
    out: &mut BufWriter<File>,
    out_path: &Path,
) -> Result<u64> {
    let arrow_err = |detail: String| BuildError::Invalid(format!("{rel}: {detail}"));
    let reader = ArrowFileReader::try_new(std::io::Cursor::new(bytes), None)
        .map_err(|e| arrow_err(e.to_string()))?;
    let mut rows = 0u64;
    let mut previous: Vec<u8> = Vec::new();
    let mut first_over_water: Option<(u64, u32)> = None;
    for batch in reader {
        let batch = batch.map_err(|e| arrow_err(e.to_string()))?;
        let key_col = batch
            .column_by_name("external_id")
            .and_then(|c| c.as_any().downcast_ref::<BinaryArray>())
            .ok_or_else(|| arrow_err("no binary 'external_id' column".to_string()))?;
        let entity_col = batch
            .column_by_name("entity_id")
            .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
            .ok_or_else(|| arrow_err("no uint32 'entity_id' column".to_string()))?;
        for i in 0..batch.num_rows() {
            let key = key_col.value(i);
            if rows > 0 && previous.as_slice() >= key {
                return Err(BuildError::Invalid(format!(
                    "{rel}: external ids are not strictly ascending at row {rows} — the sidecar \
                     binary-searches each run, and an unsorted or duplicated key makes a binding \
                     unreachable"
                )));
            }
            previous.clear();
            previous.extend_from_slice(key);
            let entity = entity_col.value(i);
            if entity as u64 >= high_water && first_over_water.is_none() {
                first_over_water = Some((rows, entity));
            }
            out.write_all(&entity.to_le_bytes())
                .map_err(|e| BuildError::io(out_path, e))?;
            rows += 1;
        }
    }
    if let Some((i, entity)) = first_over_water {
        return Err(BuildError::Invalid(format!(
            "{rel}: row {i} binds entity {entity}, at or past entity_id_high_water {high_water}"
        )));
    }
    Ok(rows)
}
