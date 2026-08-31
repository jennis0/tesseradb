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
//! One linear pass, but it re-hashes the sidecar family and holds it resident while the two
//! agreement directions run — the *verifier's* cost model, not the serving sidecar's, whose
//! whole design is per-run laziness. Cadence is the suite's concern (§11: per-stage at the small
//! tiers, per-run above).
//!
//! ⊘ **At rest only, for now.** This resolves `CURRENT` through `open_bundle`, so a fold flipping
//! `CURRENT` mid-pass could swap the prefix under it. §12.4's answer — name the prefix once and
//! never re-read `CURRENT`, with a vanishing file reported as a race rather than a defect — needs
//! `tessera-store`'s named-prefix open made public, which has not happened. Until it has, point
//! this at a bundle no live engine is publishing into.

use std::collections::HashSet;
use std::fs::{self, File};
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
#[derive(Debug, Clone, Default)]
pub struct VerifyOpts {
    /// The points file this bundle claims to have been built from (correctness-suite §11.1).
    ///
    /// ⊘ **Specified, not implemented.** Checking it needs `source.digest` in `MANIFEST.json`,
    /// and that field belongs to contracts §2.2 — an amendment that has not been made. Passing
    /// `Some` is refused rather than silently ignored, so no harness can believe a binding was
    /// checked when nothing exists to check it against.
    pub source: Option<PathBuf>,
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
    /// `(segment, column)` pairs confirmed to hold a group-scoped **render** family's lane
    /// ([`check_scoped_render_lanes`]); 0 where no family declares `render`, which is every
    /// bundle whose attributes are entity-scoped.
    pub scoped_render_lanes: u64,
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

    let (bundle, shallow) = crate::verified_open(root)?;
    let prefix_dir = root.join(&shallow.prefix);

    let mut report = VerifyDeepReport {
        shallow,
        terms: 0,
        delta_tiers: 0,
        pairs_rows: 0,
        dict_records: 0,
        external_id_bindings: 0,
        scoped_render_lanes: 0,
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
        check_external_ids(&prefix_dir, &bundle.manifest, &partition.manifest, &mut report)?;
        check_scoped_render_lanes(&bundle.manifest, phash, partition, &mut report)?;
    }

    Ok(report)
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
            for chunk in bytes.chunks_exact(4) {
                visit(u32::from_le_bytes(chunk.try_into().expect("chunks_exact(4)")))?;
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

/// Dictionary extents positional and never repeating a descriptor (decision 0042): each extent's
/// record count must equal what the manifest declares — a term's ordinal is its position in the
/// concatenation, so a miscount shifts every later term — and no descriptor may appear twice
/// across the list, which after a restart renumbers everything past the repeat. The reader
/// (`Dict::load`) tolerates a repeat by skipping it, deliberately; this is the artefact-side
/// statement that no correct writer produces one.
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

/// One external-id run, fully decoded and held resident — the verifier's cost model, not the
/// serving sidecar's (see the module doc).
struct RunRows {
    rel: String,
    keys: Vec<Vec<u8>>,
    entities: Vec<u32>,
}

/// One locator, decoded: the base file over `[0, len)`, or a flush extent over its own span,
/// with the run its ordinals index.
struct ExtentSlots {
    rel: String,
    entity_lo: u64,
    entity_hi: u64,
    run: usize,
    slots: Vec<u32>,
}

/// The external-id locator and its sidecar agreeing in both directions, newest binding first —
/// see the module doc for the exact invariant and for why a repeated key across runs is the
/// accepted state rather than a defect.
fn check_external_ids(
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

    // The sidecar family is exempt from the open-time digest sweep (deviation 9), so this pass
    // digests each file against the manifest before believing a byte of it.
    let read_verified = |rel: &str| -> Result<Vec<u8>> {
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
        let bytes = fs::read(&path).map_err(|e| BuildError::io(&path, e))?;
        if bytes.len() as u64 != digest.size
            || crate::hex_digest(Sha256::digest(&bytes).as_slice()) != digest.sha256
        {
            return Err(BuildError::Invalid(format!(
                "{rel}: bytes do not match the manifest digest — this family is exempt from the \
                 open-time sweep (contracts §0.3 deviation 9), so the deep pass is where a \
                 corrupt sidecar file is caught at rest"
            )));
        }
        Ok(bytes)
    };

    let mut runs: Vec<RunRows> = Vec::with_capacity(runs_rel.len());
    for rel in runs_rel {
        runs.push(decode_run(rel, read_verified(rel)?, high_water)?);
    }
    // Concatenation offsets, in listed order: what a base-locator ordinal addresses.
    let mut offsets: Vec<u64> = Vec::with_capacity(runs.len() + 1);
    let mut total = 0u64;
    for run in &runs {
        offsets.push(total);
        total += run.keys.len() as u64;
    }

    // The base locator lives beside the first run under the same derivation the sidecar uses.
    let locator_rel = match runs_rel[0].rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/ext-locator.u32"),
        None => "ext-locator.u32".to_string(),
    };
    let base_len = bundle_manifest.entity_id_high_water;
    let base = decode_locator(&locator_rel, read_verified(&locator_rel)?, base_len)?;

    let mut extents: Vec<ExtentSlots> = Vec::with_capacity(partition_manifest.locator_extents.len());
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
        let slots = decode_locator(&extent.path, read_verified(&extent.path)?, span)?;
        extents.push(ExtentSlots {
            rel: extent.path.clone(),
            entity_lo: extent.entity_lo,
            entity_hi: extent.entity_hi,
            run,
            slots,
        });
    }

    // Direction one: every locator slot addresses a row bound to exactly its entity.
    for (entity, &slot) in base.iter().enumerate() {
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
        let run = offsets.partition_point(|&start| start <= global) - 1;
        let local = (global - offsets[run]) as usize;
        let bound = runs[run].entities[local] as u64;
        if bound != entity as u64 {
            return Err(BuildError::Invalid(format!(
                "{locator_rel}: entity {entity}'s slot addresses a row of '{}' bound to entity \
                 {bound} — the locator and the runs disagree",
                runs[run].rel
            )));
        }
    }
    for extent in &extents {
        for (i, &slot) in extent.slots.iter().enumerate() {
            if slot == LOCATOR_NONE {
                continue;
            }
            let entity = extent.entity_lo + i as u64;
            let run = &runs[extent.run];
            let Some(&bound) = run.entities.get(slot as usize) else {
                return Err(BuildError::Invalid(format!(
                    "{}: entity {entity}'s ordinal {slot} is past the end of run '{}' ({} rows)",
                    extent.rel,
                    run.rel,
                    run.entities.len()
                )));
            };
            if bound as u64 != entity {
                return Err(BuildError::Invalid(format!(
                    "{}: entity {entity}'s slot addresses a row of '{}' bound to entity {bound} \
                     — the locator and the runs disagree",
                    extent.rel, run.rel
                )));
            }
        }
    }

    // Direction two: every run row is addressed back by its own entity's covering slot. This is
    // what refuses a dangling binding (a row no locator can reach) and, with direction one, two
    // slots claiming one row. A key bound in several runs passes both directions: each of its
    // rows has its own entity, and each entity its own slot.
    for (r, run) in runs.iter().enumerate() {
        for (i, &entity) in run.entities.iter().enumerate() {
            let entity = entity as u64;
            let agrees = if entity < base_len {
                base[entity as usize] != LOCATOR_NONE
                    && base[entity as usize] as u64 == offsets[r] + i as u64
            } else if let Some(extent) = extents
                .iter()
                .find(|x| entity >= x.entity_lo && entity <= x.entity_hi)
            {
                extent.run == r
                    && extent.slots[(entity - extent.entity_lo) as usize] == i as u32
            } else {
                return Err(BuildError::Invalid(format!(
                    "{}: row {i} binds entity {entity}, which no locator covers — the reverse \
                     direction could never answer for it",
                    run.rel
                )));
            };
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

/// Decode one external-id run: `(external_id: Binary, entity_id: UInt32)`, keys strictly
/// ascending within the run (a duplicate inside one run would make a binding unreachable to the
/// sidecar's binary search — across runs is the accepted 0047 state), entities below
/// `high_water`.
fn decode_run(rel: &str, bytes: Vec<u8>, high_water: u64) -> Result<RunRows> {
    let arrow_err = |detail: String| BuildError::Invalid(format!("{rel}: {detail}"));
    let reader = ArrowFileReader::try_new(std::io::Cursor::new(bytes), None)
        .map_err(|e| arrow_err(e.to_string()))?;
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut entities: Vec<u32> = Vec::new();
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
            keys.push(key_col.value(i).to_vec());
            entities.push(entity_col.value(i));
        }
    }
    for (i, window) in keys.windows(2).enumerate() {
        if window[0] >= window[1] {
            return Err(BuildError::Invalid(format!(
                "{rel}: external ids are not strictly ascending at row {} — the sidecar \
                 binary-searches each run, and an unsorted or duplicated key makes a binding \
                 unreachable",
                i + 1
            )));
        }
    }
    for (i, &entity) in entities.iter().enumerate() {
        if entity as u64 >= high_water {
            return Err(BuildError::Invalid(format!(
                "{rel}: row {i} binds entity {entity}, at or past entity_id_high_water \
                 {high_water}"
            )));
        }
    }
    Ok(RunRows {
        rel: rel.to_string(),
        keys,
        entities,
    })
}

/// Decode a locator file: a raw little-endian `u32` array, no header, exactly `declared_len`
/// entries.
fn decode_locator(rel: &str, bytes: Vec<u8>, declared_len: u64) -> Result<Vec<u32>> {
    if bytes.len() as u64 != declared_len * 4 {
        return Err(BuildError::Invalid(format!(
            "{rel}: {} bytes where the manifest-implied length is {declared_len} u32 slots \
             ({} bytes)",
            bytes.len(),
            declared_len * 4
        )));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("chunks_exact(4)")))
        .collect())
}
