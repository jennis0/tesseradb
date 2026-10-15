//! `MANIFEST.json` and `CURRENT` — the last two bundle artefacts, and the ones a fold's
//! publication (compaction §4 step 4) needs a writer for.
//!
//! **A bundle artefact, so its writer lives with the others** (owner ruling, 2026-08-06;
//! compaction §10's rule paragraph): `SegmentWriter`, `PermutationWriter` and
//! `PairsParquetWriter` (see [`crate::pairs`]) all sit in this crate because
//! they write files contracts §2 defines, and `MANIFEST.json` wrote from `mosaica-build` alone
//! for as long as a build was the only thing that produced it. Compaction's pass 5 is the second
//! producer, and it cannot reach `mosaica-build`: the fold's driver lives in `mosaica-engine`,
//! which has no edge to `mosaica-build`, and `mosaica-build` already depends on `mosaica-authz` —
//! so routing the fold's manifest write through it would be the reverse edge, a cycle cargo
//! refuses. Moving the writer here is the placement the other bundle artefacts already have,
//! rather than a new exception for this one.
//!
//! `mosaica-build` is the caller now, not a second implementation: two independent serialisers
//! of the same JSON shape is exactly how a build's bundle and a fold's bundle would come to
//! disagree about what a given manifest digests to.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};
use crate::layout::{segments_name, LayerSpaceLayout, ShardLayout};
use crate::manifest::{
    carries_deny, CurrentPointer, Manifest, PartitionFile, PublishedRefs, RecordFile,
    SegmentsManifest, ShardFiles, ShardRef, SpaceRef,
};
use crate::read::hex_sha256;

/// Serialise `manifest` to `<prefix_dir>/MANIFEST.json` and return the hex SHA-256 of the exact
/// bytes written.
///
/// **The returned digest is computed from the bytes handed to the writer, not from a re-read
/// afterwards, and serialisation happens exactly once.** `CURRENT.manifest_digest` must equal
/// the SHA-256 of `MANIFEST.json`'s bytes (contracts §2.1) — `open_bundle` checks it on every
/// open — and that same digest is the bundle identity that each shard's identity, and so its
/// mask fragments' key, is derived from. A second serialisation could legally produce different
/// bytes (map key order is stable here because `Manifest::files` is a `BTreeMap`, but nothing
/// about *this* function should have to reason about that to stay correct), and a re-read after
/// write pays disc I/O to learn something the write already knows. Serialise once, hash that,
/// write that.
///
/// **`serde_json::to_vec_pretty`, matching `mosaica-build`'s writer exactly.** `to_vec` produces
/// different bytes for the same value — different whitespace, same digest input, different
/// digest — and a bundle on disc today was written with the pretty form. Changing it would not
/// break anything this function checks; it would silently change every future bundle's identity
/// against every bundle already written, which is the one thing a format's own writer must never
/// do without a `bundle_format` bump.
///
/// MANIFEST.json is written directly, not through a temporary file. It is immutable once
/// written (contracts §2.1), but nothing reads it until `CURRENT` names the prefix it sits
/// under — `write_current` is the commit point — so a crash mid-write here leaves a partial file
/// under a prefix `CURRENT` does not yet name: an orphan, indistinguishable from any other
/// partially-published prefix and swept the same way (compaction §3, pass 5's note on discarded
/// folds). What must not be skipped is fsyncing the directory after the file: a file's data can
/// be durable on disc while its directory entry is not, and `write_current` is about to record
/// this file's digest as the thing a durable `CURRENT` vouches for — the manifest must be
/// durably *nameable* before anything durable points at it.
pub fn write_manifest_json(prefix_dir: &Path, manifest: &Manifest) -> Result<String> {
    let path = prefix_dir.join("MANIFEST.json");
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|source| StoreError::Json {
        path: path.clone(),
        source,
    })?;
    write_and_fsync(&path, &bytes)?;
    fsync_dir(prefix_dir)?;
    Ok(hex_sha256(&bytes))
}

/// Write `<bundle_root>/CURRENT`, naming `prefix` and `manifest_digest`. **The commit point**
/// (contracts §2.1): every reader's whole trust chain starts here, so this file must be durable
/// before it is visible and must never be observed half-written.
///
/// Write-then-rename, in this order:
///
/// 1. Serialise `{prefix, manifest_digest}` and write it to `CURRENT.tmp`, then `fsync` that
///    file. This makes the *bytes* durable while the name `CURRENT` still points at whatever
///    bundle was live before this call — a crash here leaves `CURRENT.tmp` as an orphan and
///    changes nothing a reader can see.
/// 2. `rename(2)` `CURRENT.tmp` to `CURRENT`. On the local filesystems this format targets,
///    `rename` onto an existing name is atomic: a reader that opens `CURRENT` mid-call sees
///    either the old bytes or the new ones, never a mix, and never a missing file.
/// 3. `fsync` the containing directory. The rename updates a directory entry, and a directory
///    entry is not guaranteed durable until the directory holding it is synced — without this,
///    a crash immediately after a returned `rename()` could roll `CURRENT` back to the prior
///    bundle on the next mount, silently un-publishing whatever this call just committed.
///
/// The `fsync` in step 1 has to precede the rename: syncing the directory *before* the file's
/// own bytes are durable would make the name durable while the data behind it might not be,
/// which is the half-written state this function exists to rule out. The order the other way —
/// data durable, then the name that points at it — is the only one with no window in which a
/// durable `CURRENT` can name bytes that a crash could still lose.
pub fn write_current(bundle_root: &Path, prefix: &str, manifest_digest: &str) -> Result<()> {
    let current = CurrentPointer {
        prefix: prefix.to_string(),
        manifest_digest: manifest_digest.to_string(),
    };
    let tmp_path = bundle_root.join("CURRENT.tmp");
    let bytes = serde_json::to_vec_pretty(&current).map_err(|source| StoreError::Json {
        path: tmp_path.clone(),
        source,
    })?;
    write_and_fsync(&tmp_path, &bytes)?;

    let current_path = bundle_root.join("CURRENT");
    fs::rename(&tmp_path, &current_path).map_err(|source| StoreError::Io {
        path: current_path,
        source,
    })?;
    fsync_dir(bundle_root)
}

/// Create `path`, write `bytes`, and `fsync` the file — the data half of durability. Callers
/// that also need the directory entry durable follow up with [`fsync_dir`].
pub fn write_and_fsync(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// `fsync` every file in `paths` **and every directory that holds one**, so both the bytes and the
/// names that reach them are durable.
///
/// # Why a caller needs this at all
///
/// The segment and postings writers do not sync: `write_single_batch` says so at the
/// site, and the reasoning has always been that a partially-written file is *detectable* — the
/// manifest digests catch it, and the producer re-runs. That holds for a build (nothing else has
/// been deleted yet) and for a flush (the WAL still holds the rows, and the side-manifest it was
/// committed under can be stepped past). **It does not hold for a fold**, which flips `CURRENT` onto
/// the new prefix and then deletes the old tree and reclaims the WAL members behind it. After that
/// the folded bytes are the only copy, so "detectable" becomes "detectably gone".
///
/// Ordering matters and is the caller's: this must complete **before** `CURRENT` names the prefix
/// these files sit under. Afterwards is too late by exactly the window it exists to close.
///
/// **Both callers are the fold's, and the second is the non-obvious one.** Pass 5 syncs the files
/// the fold itself wrote; publication syncs the ones it *hard-linked* — where a link copies no
/// bytes, so the directory entry is plainly new and the bytes look as though they must already be
/// durable. They are not: no producer in this tree fsyncs a data file, which is sound everywhere
/// else (a torn file is detectable through its digest and its rows are still in the WAL) and unsound
/// for a fold, which deletes the other copy and rotates the WAL behind it.
///
/// ⊘ **Not covered by a test, and nothing here could cover it.** An `fsync` has no in-process
/// observable — a caller that skipped it passes every assertion in this tree, because the page cache
/// answers reads identically either way. What would cover it is crash injection below the
/// filesystem, which nothing here has. The property is argued at the call sites instead.
///
/// Directories are deduplicated and synced after the files they hold, because a directory entry is
/// not durable until its directory is and the entry must not outlive the data it names.
pub fn fsync_written(paths: &[PathBuf]) -> Result<()> {
    let mut dirs: Vec<&Path> = Vec::new();
    for path in paths {
        // Read-only is enough: `fsync(2)` flushes the *file*, not the descriptor's access mode, and
        // this is the same open `pairs.rs` already uses for its own sync.
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        if let Some(dir) = path.parent() {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    for dir in dirs {
        fsync_dir(dir)?;
    }
    Ok(())
}

pub fn fsync_dir(path: &Path) -> Result<()> {
    let dir = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    dir.sync_all().map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Write publication `n` of `partition`: [`stage_publication`], then [`StagedPublication::commit`].
pub fn write_publication(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    manifest: &SegmentsManifest,
    previous: Option<&PublishedRefs>,
) -> Result<PublishedRefs> {
    stage_publication(prefix_dir, partition, n, manifest, previous)?.commit()
}

/// A publication whose shard and space files are durable and whose partition file, the commit
/// point, is not yet written. Nothing names the staged files until [`Self::commit`] returns.
#[must_use = "a staged publication commits nothing until `commit` is called"]
pub struct StagedPublication {
    dir: PathBuf,
    n: u64,
    bytes: Vec<u8>,
    refs: PublishedRefs,
}

impl StagedPublication {
    /// Write the partition's `SEGMENTS-<n>.json`, which commits the publication.
    pub fn commit(self) -> Result<PublishedRefs> {
        write_once(&self.dir, &segments_name(self.n), &self.bytes)?;
        Ok(self.refs)
    }
}

/// Write each shard's and each layer space's `SEGMENTS-<n>.json` of `manifest` whose bytes differ
/// from the file `previous` names for it, fsynced with their directories, and return the
/// partition file still to be written. A file whose bytes match keeps the reference, and so the
/// number, of the publication that wrote it. `previous` is the newest publication's references,
/// and `None` writes every file, as a new prefix needs.
pub fn stage_publication(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    manifest: &SegmentsManifest,
    previous: Option<&PublishedRefs>,
) -> Result<StagedPublication> {
    let mut split = manifest.split_files(partition);
    let mut refs = PublishedRefs::default();
    for record in &manifest.shards.records {
        let files = split.shards.remove(&record.id).unwrap_or_default();
        let dir = prefix_dir.join(ShardLayout::new(partition, record.id).rel());
        let kept = previous
            .and_then(|previous| previous.shards.iter().find(|named| named.id == record.id))
            .map(|named| (named.n, named.size, named.sha256.as_str()));
        let (written_at, size, sha256) = stage_file(
            &dir,
            n,
            &RecordFile {
                record,
                files: &files,
            },
            kept,
        )?;
        refs.shards.push(ShardRef {
            id: record.id,
            n: written_at,
            size,
            sha256,
            denies: carries_deny(&record.tombstones, &record.deny),
        });
    }
    for record in &manifest.layer_spaces {
        let files = split.layer_spaces.remove(&record.space).unwrap_or_default();
        let dir = prefix_dir.join(LayerSpaceLayout::new(partition, record.space).rel());
        let kept = previous
            .and_then(|previous| {
                previous
                    .layer_spaces
                    .iter()
                    .find(|named| named.space == record.space)
            })
            .map(|named| (named.n, named.size, named.sha256.as_str()));
        let (written_at, size, sha256) = stage_file(
            &dir,
            n,
            &RecordFile {
                record,
                files: &files,
            },
            kept,
        )?;
        refs.layer_spaces.push(SpaceRef {
            space: record.space,
            n: written_at,
            size,
            sha256,
            denies: carries_deny(&record.tombstones, &record.deny),
        });
    }
    let mut fields = manifest.partition.clone();
    fields.files = split.partition;
    let file = PartitionFile {
        partition: fields,
        shards: ShardFiles {
            size: manifest.shards.size,
            next_id: manifest.shards.next_id,
            files: refs.shards.clone(),
        },
        layer_spaces: refs.layer_spaces.clone(),
    };
    let dir = crate::layout::partition_dir(prefix_dir, partition);
    let bytes = to_json(&dir.join(segments_name(n)), &file)?;
    Ok(StagedPublication {
        dir,
        n,
        bytes,
        refs,
    })
}

/// Write `value` as `dir`'s `SEGMENTS-<n>.json`, unless its bytes match `kept`, the size and
/// digest of the file the previous publication named, and return the `(n, size, sha256)` the
/// partition file names: `kept`'s where they match.
fn stage_file(
    dir: &Path,
    n: u64,
    value: &impl serde::Serialize,
    kept: Option<(u64, u64, &str)>,
) -> Result<(u64, u64, String)> {
    let name = segments_name(n);
    let bytes = to_json(&dir.join(&name), value)?;
    let (size, sha256) = (bytes.len() as u64, hex_sha256(&bytes));
    match kept {
        Some((kept_n, kept_size, kept_sha256)) if kept_size == size && kept_sha256 == sha256 => {
            Ok((kept_n, size, sha256))
        }
        _ => {
            write_once(dir, &name, &bytes)?;
            Ok((n, size, sha256))
        }
    }
}

fn to_json(path: &Path, value: &impl serde::Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `name` in `dir`, fsynced, and fsync `dir`, refusing to replace a file of that name.
///
/// **A file named for a publication is never replaced, and that is a safety property.** Each
/// `SEGMENTS-<n>.json` is complete current state for what it describes, not a diff, so a second
/// writer at the same `n` would not merge with the first but replace it, and rows acknowledged
/// and published under the first would be absent from the manifest a restart opens.
///
/// **`hard_link` rather than `rename`, because `std` has no `RENAME_NOREPLACE`.** `rename(2)`
/// replaces silently; `link(2)` fails with `AlreadyExists` and is equally atomic, so a name
/// existing at all means its file is complete. A crash between the link and the unlink leaves a
/// `.tmp` orphan, which nothing reads.
fn write_once(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(name);
    let io = |what: &str, source: std::io::Error| StoreError::Io {
        path: PathBuf::from(format!("{} ({what})", path.display())),
        source,
    };
    fs::create_dir_all(dir).map_err(|e| io("create the directory", e))?;
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut file = File::create(&tmp).map_err(|e| io("create", e))?;
        file.write_all(bytes).map_err(|e| io("write", e))?;
        file.sync_all().map_err(|e| io("fsync", e))?;
    }
    let linked = std::fs::hard_link(&tmp, &path);
    // The temporary is consumed either way, so a refused write leaves nothing for the next
    // attempt at this name to trip over.
    let _ = std::fs::remove_file(&tmp);
    linked.map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => StoreError::SideManifestExists { path: path.clone() },
        _ => io("link (a side-manifest is never replaced)", e),
    })?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io("dir fsync", e))?;
    Ok(())
}

/// How many `SEGMENTS-<n>.json` one partition directory keeps: the newest and the two before it.
pub const SIDE_MANIFESTS_KEPT: usize = 3;

/// Delete every partition `SEGMENTS-<n>.json` below the [`SIDE_MANIFESTS_KEPT`] highest, then
/// every shard's and layer space's `SEGMENTS-<m>.json` that no kept partition file names and whose
/// `m` is below the highest kept `n`, and return how many files were removed.
///
/// **Deleting a superseded candidate removes it from the step-down walk rather than letting the
/// walk skip it, and that is what keeps the walk safe.** The walk's safety rests on never stepping
/// *past* a deny-carrying manifest unexamined, and a publication is complete current state rather
/// than a diff: the newest one restates the whole deny state, so nothing a pruned one carried is
/// absent from the ones kept. What a reader loses is depth to step down into.
///
/// **The highest `n` is never deleted**, whoever wrote it: the next number a writer may take is
/// derived from the names present. A shard or space file at or above it is a publication's that
/// has not committed yet, and is left. Where a kept partition file cannot be read, no shard or
/// space file is removed, since what it names is unknown.
///
/// Directory entries are left unsynced: a deletion lost to a crash is pruned again.
pub fn prune_superseded_segments_manifests(prefix_dir: &Path, partition: &str) -> Result<usize> {
    let dir = crate::layout::partition_dir(prefix_dir, partition);
    let mut present = crate::read::list_segments_manifests(&dir, partition)?;
    // Highest first, so the skip below keeps the newest and the two before it.
    present.sort_unstable_by(|a, b| b.cmp(a));
    let mut removed = 0;
    for n in present.iter().skip(SIDE_MANIFESTS_KEPT) {
        removed += remove_if_present(&dir.join(segments_name(*n)))?;
    }
    let Some(&highest) = present.first() else {
        return Ok(removed);
    };
    let mut named = std::collections::BTreeSet::new();
    for n in present.iter().take(SIDE_MANIFESTS_KEPT) {
        let Ok(file) = crate::read::read_partition_file(&dir.join(segments_name(*n))) else {
            return Ok(removed);
        };
        for shard in &file.shards.files {
            named.insert(ShardLayout::new(partition, shard.id).manifest_rel(shard.n));
        }
        for space in &file.layer_spaces {
            named.insert(LayerSpaceLayout::new(partition, space.space).manifest_rel(space.n));
        }
    }
    for kind in [
        crate::layout::shards_rel(partition),
        crate::layout::layer_spaces_rel(partition),
    ] {
        for (rel, m) in crate::read::numbered_files_under(prefix_dir, &kind)? {
            if m < highest && !named.contains(&rel) {
                removed += remove_if_present(&prefix_dir.join(&rel))?;
            }
        }
    }
    Ok(removed)
}

/// Remove `path`, counting it: one already gone is not a failure to remove it.
fn remove_if_present(path: &Path) -> Result<usize> {
    match fs::remove_file(path) {
        Ok(()) => Ok(1),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}
