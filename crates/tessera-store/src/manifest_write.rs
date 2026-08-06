//! `MANIFEST.json` and `CURRENT` — the last two bundle artefacts, and the ones a fold's
//! publication (compaction §4 step 4) needs a writer for.
//!
//! **A bundle artefact, so its writer lives with the others** (owner ruling, 2026-08-06;
//! compaction §10's rule paragraph): `SegmentWriter`, `PermutationWriter`, `RunWriter`,
//! `LocatorWriter` and `PairsParquetWriter` (see [`crate::pairs`]) all sit in this crate because
//! they write files contracts §2 defines, and `MANIFEST.json` wrote from `tessera-build` alone
//! for as long as a build was the only thing that produced it. Compaction's pass 5 is the second
//! producer, and it cannot reach `tessera-build`: the fold's driver lives in `tessera-engine`,
//! which has no edge to `tessera-build`, and `tessera-build` already depends on `tessera-authz` —
//! so routing the fold's manifest write through it would be the reverse edge, a cycle cargo
//! refuses. Moving the writer here is the placement the other bundle artefacts already have,
//! rather than a new exception for this one.
//!
//! `tessera-build` is the caller now, not a second implementation: two independent serialisers
//! of the same JSON shape is exactly how a build's bundle and a fold's bundle would come to
//! disagree about what a given manifest digests to.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Result, StoreError};
use crate::manifest::{CurrentPointer, Manifest, SegmentsManifest};

/// Serialise `manifest` to `<prefix_dir>/MANIFEST.json` and return the hex SHA-256 of the exact
/// bytes written.
///
/// **The returned digest is computed from the bytes handed to the writer, not from a re-read
/// afterwards, and serialisation happens exactly once.** `CURRENT.manifest_digest` must equal
/// the SHA-256 of `MANIFEST.json`'s bytes (contracts §2.1) — `open_bundle` checks it on every
/// open — and that same digest is the bundle identity that keys every mask fragment
/// (compaction §4's `bundle_identity`). A second serialisation could legally produce different
/// bytes (map key order is stable here because `Manifest::files` is a `BTreeMap`, but nothing
/// about *this* function should have to reason about that to stay correct), and a re-read after
/// write pays disc I/O to learn something the write already knows. Serialise once, hash that,
/// write that.
///
/// **`serde_json::to_vec_pretty`, matching `tessera-build`'s writer exactly.** `to_vec` produces
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
fn write_and_fsync(path: &Path, bytes: &[u8]) -> Result<()> {
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

fn fsync_dir(path: &Path) -> Result<()> {
    let dir = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    dir.sync_all().map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Write `SEGMENTS-<n>.json`, fsynced, and fsync its directory entry.
///
/// **This is the commit point** (§7.3). Everything it names is already durable; a crash before the
/// link leaves orphan files nothing references, and a crash after it leaves a bundle that opens
/// at `n` with everything present.
///
/// **It refuses to replace an existing `SEGMENTS-<n>.json`, and that is a safety property.** A
/// side-manifest is complete current state for its partition, not a diff, so a second writer at
/// the same `n` does not merge with the first — it *replaces* it, with a manifest built from the
/// same base and naming only its own segment. The winner's rows would then be absent from the
/// manifest a restart opens, having been acked and published: silent loss of exactly the kind the
/// rest of this module is built to prevent. Two plans in one dispatch share `next_n` and would do
/// this; `dispatch_flushes` now sends one, and this is the guard at the artefact rather than at
/// the caller — the same belt-and-braces the dictionary's no-duplicate rule takes, and for the
/// same reason: a rule held by one caller is rediscovered from prose by the next.
///
/// **`hard_link` rather than `rename`, because `std` has no `RENAME_NOREPLACE`.** `rename(2)`
/// replaces silently; `link(2)` fails with `AlreadyExists` and is equally atomic, so the reader's
/// property — `SEGMENTS-<n>.json` existing at all means it is complete — is unchanged. A crash
/// between the link and the unlink leaves a `.tmp` orphan, which is the same orphan story every
/// stage before this one already accepts.
pub fn write_segments_manifest(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    manifest: &SegmentsManifest,
) -> Result<()> {
    let dir = prefix_dir.join("partitions").join(partition);
    let path = dir.join(format!("SEGMENTS-{n}.json"));
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|source| StoreError::Json { path: path.clone(), source })?;
    let io = |what: &str, source: std::io::Error| StoreError::Io {
        path: PathBuf::from(format!("{} ({what})", path.display())),
        source,
    };

    // Written to a temporary sibling and linked into place, so a reader walking the candidate list
    // never sees a partial one: `SEGMENTS-<n>.json` existing at all must mean it is complete.
    let tmp = dir.join(format!("SEGMENTS-{n}.json.tmp"));
    {
        let mut file = File::create(&tmp).map_err(|e| io("create", e))?;
        file.write_all(&bytes).map_err(|e| io("write", e))?;
        file.sync_all().map_err(|e| io("fsync", e))?;
    }
    // Refuse-to-replace — see this function's doc for why this is not a rename.
    let linked = std::fs::hard_link(&tmp, &path);
    // The temporary is consumed either way: on success it has a second name, on refusal it is
    // rubbish. Unlinked before the error surfaces so a refused write leaves nothing behind for the
    // next attempt at this `n` to trip over.
    let _ = std::fs::remove_file(&tmp);
    linked.map_err(|e| io("link (a side-manifest is never replaced)", e))?;
    File::open(&dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io("dir fsync", e))?;
    Ok(())
}
