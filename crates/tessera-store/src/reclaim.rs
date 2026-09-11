//! The fold's two irreversible filesystem primitives (compaction §8): carrying files forward
//! into the new prefix, and deleting the old one whole.
//!
//! **Why hard links, and why that is the whole point.** A fold's snapshot folds most of the
//! corpus into new segments, tiers and runs, but not all of it — flushes publish into the *old*
//! prefix throughout the fold's flight (compaction §1), so post-snapshot segments, tiers,
//! external-id runs and locator extents, plus the dictionary extents (carried **verbatim**,
//! never renumbered — compaction §3 pass 4), have to reach the new prefix unchanged.
//! [`hard_link_forward`] does that by adding a second directory entry for the same inode rather
//! than copying bytes. That is what makes [`reclaim_prefix`]'s later whole-tree delete safe at
//! all: once every file the new prefix still needs has its own directory entry there, deleting
//! the old prefix's tree unlinks *directory entries*, never live data. A copy would instead
//! double the disc the fold already doubles (compaction §8: "peak disc is old prefix + new
//! prefix — roughly 2× live bytes"), and on an object store — where there is no hard link and
//! the operation really is a copy — the fold's disc estimate has to account for that difference
//! rather than assume the local-filesystem shape.
//!
//! Both primitives are narrow on purpose. Neither decides *which* files to carry forward or
//! *when* the old prefix has no readers left holding a mapping of it — that is compaction §4's
//! publication order and §8's "once no request holds a mapping" respectively, decided by the
//! executor. This module only makes the two mechanical steps safe to call: a link that cannot
//! escape either prefix and never silently overwrites, and a delete that refuses whenever it
//! cannot prove the target is not the live prefix.
//!
//! **Two deletes, two proofs, one removal.** [`reclaim_prefix`] proves a prefix is not live by
//! reading `CURRENT` and finding another prefix named there. [`reclaim_unpublished_prefix`] proves
//! it by finding no `CURRENT` at all, which is the state a bundle directory is in while a build is
//! still writing it and stays in if that build fails. Each refuses on anything short of its own
//! proof, and both go through the same removal.

use std::path::Path;

use crate::error::{read_to_vec, Result, StoreError};
use crate::manifest::CurrentPointer;
// **The one path-escape rule**, borrowed rather than restated: `rels` are manifest `files`-map
// keys, and a second definition of what counts as a safe manifest path is a second thing to get
// right on the boundary that keeps a link inside the bundle root.
use crate::read::safe_join;

/// Hard-link each prefix-relative path in `rels` from `from_prefix` into `to_prefix`, creating
/// parent directories under `to_prefix` as needed.
///
/// Used **before `CURRENT` flips** (compaction §4 step 4, and pass 5's own carry-forward
/// linking, §3): every carry-forward category in compaction §2's table — post-snapshot
/// segments, delta tiers, external-id runs and locator extents, and the dictionary extents — is
/// linked into the new prefix by prefix-relative path, at either or both of those call sites
/// (publication links in whatever arrived after pass 5's own pass). A hard link changes nothing
/// about a file's content or its digest, only how many directory entries name the same inode, so
/// the digest a carried-forward file earned under the old prefix's path is still correct once
/// `MANIFEST.json` records the same bytes under the new one — nothing needs to be re-hashed.
///
/// # Errors
///
/// - [`StoreError::UnsafePath`] if an entry cannot be safely joined onto either prefix (see
///   [`safe_join`]) — a typed error before any filesystem call, never a link that landed outside
///   `to_prefix`.
/// - [`StoreError::MalformedBundle`] if the link target already exists. That is a bug in the
///   caller — two passes writing one path (contracts §2.1: a `seg_id` or `coalesced/<id>` is
///   never reused for exactly this reason) — not a case to paper over by skipping or
///   overwriting; overwriting a hard-linked file rewrites every path that names the same inode,
///   including whatever the old prefix still has open.
/// - [`StoreError::MalformedBundle`] if the link would cross filesystems (`EXDEV`). A prefix
///   lives inside one bundle root, so this should never arise in practice, but the raw errno is
///   uninformative on its own, so it is translated to name what happened.
/// - [`StoreError::Io`] for any other filesystem failure (missing source, permissions, …).
pub fn hard_link_forward(from_prefix: &Path, to_prefix: &Path, rels: &[String]) -> Result<()> {
    for rel in rels {
        let from_path = safe_join(from_prefix, rel)?;
        let to_path = safe_join(to_prefix, rel)?;

        if let Some(parent) = to_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        // Checked ahead of the link call so the error names the caller bug in its own words
        // rather than however `hard_link`'s `AlreadyExists` prints — and so a symlink or
        // directory squatting the target path is caught the same way a plain file would be.
        if to_path.exists() {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "hard_link_forward: target already exists at {} — linking must not \
                     overwrite; this means two passes wrote the same path",
                    to_path.display()
                ),
            });
        }

        std::fs::hard_link(&from_path, &to_path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::CrossesDevices {
                StoreError::MalformedBundle {
                    detail: format!(
                        "hard_link_forward: {} and {} are on different filesystems (EXDEV); a \
                         prefix lives inside one bundle root, so linking {} from {} to {} \
                         should never cross devices — {source}",
                        from_path.display(),
                        to_path.display(),
                        rel,
                        from_prefix.display(),
                        to_prefix.display(),
                    ),
                }
            } else {
                StoreError::Io {
                    path: to_path.clone(),
                    source,
                }
            }
        })?;
    }
    Ok(())
}

/// Delete a whole prefix tree — the fold's reclamation event (compaction §8).
///
/// By the time this is called, every file the new prefix still needs has been hard-linked into
/// it ([`hard_link_forward`]), so this delete unlinks directory entries only — the build's
/// original base, every merged-away segment, every consumed tier, every superseded
/// side-manifest — and never touches bytes the live prefix names. That is the property
/// compaction §8 states ("no manifest names it, no WAL record depends on it") and the property
/// this module's tests assert by inode rather than by "the file still exists": a copy would
/// pass the weaker check too, and would have doubled the disc the fold already doubles.
///
/// # The one check that makes this safe to call
///
/// Nothing else in this codebase deletes a file. A wrong `prefix_dir` here is unrecoverable
/// data loss, so before removing anything this derives the bundle root (`prefix_dir`'s parent)
/// and the prefix name (`prefix_dir`'s own final component), reads that root's `CURRENT`, and
/// refuses — loudly, as a typed error, never a silent no-op — if `CURRENT` still names this
/// prefix as live. `CURRENT` is read fresh here rather than trusted from an argument: the
/// caller's only correct invocation is "after the swap, after the WAL rotation" (compaction §1's
/// diagram — reclamation is the last step, run once retirement is durable), and `CURRENT` is the
/// one artefact that can actually say whether that has happened, being the bundle's sole
/// mutable file and its commit point (contracts §2.1).
///
/// An unreadable, unparsable or structurally incomplete `CURRENT` also refuses: this function
/// cannot prove `prefix_dir` is safe to delete without it, and a delete gets fail-closed, not
/// fail-open, on any doubt.
///
/// # Errors
///
/// - [`StoreError::ReclaimRefused`] if `CURRENT` still names this prefix as live — a typed
///   refusal rather than free text, because this is the only operation here that deletes bundle
///   data and a caller must be able to tell that nothing was deleted.
/// - [`StoreError::MalformedBundle`] if `prefix_dir` has no file name or no parent to read
///   `CURRENT` from.
/// - [`StoreError::Io`] / [`StoreError::Json`] if `CURRENT` cannot be read or parsed.
/// - [`StoreError::Io`] if the delete itself fails.
pub fn reclaim_prefix(prefix_dir: &Path) -> Result<()> {
    let (prefix_name, bundle_root) = prefix_parts(prefix_dir, "reclaim_prefix")?;
    let current_path = bundle_root.join("CURRENT");
    let current_bytes = read_to_vec(&current_path)?;
    let current: CurrentPointer =
        serde_json::from_slice(&current_bytes).map_err(|source| StoreError::Json {
            path: current_path.clone(),
            source,
        })?;

    if current.prefix == prefix_name {
        return Err(StoreError::ReclaimRefused {
            prefix: prefix_name.to_string(),
            current: current.prefix,
        });
    }

    remove_tree(prefix_dir)
}

/// Delete a prefix tree **no `CURRENT` has ever named** — the partial bundle a failed build leaves.
///
/// A batch build writes its prefix under `<out>/` and writes `CURRENT` last, and it refuses to
/// start at all where `<out>/CURRENT` already exists (`tessera_build`'s argument validation), so
/// after a build has failed there is no `CURRENT` in that root. No manifest names the prefix, no
/// reader can resolve it, and nothing has been published from it. Deleting it is what stops a
/// retry starting with less free space than the first attempt had.
///
/// **The exception is a second build into the same `--out`.** Nothing locks a bundle root, so two
/// builds can be writing `<out>/v00000` at once, and the one that fails first deletes the other's
/// tree from under it. What that costs is the running build, which fails on a file that went away.
/// It is not a new hazard — two builds into one `--out` were already writing over each other's
/// files — and it is not a published byte: `CURRENT` is still absent, so neither build has
/// published anything a reader can reach.
///
/// # The check that makes this safe to call
///
/// The absence of `CURRENT` is the whole proof, so it is tested rather than assumed: this derives
/// the bundle root from `prefix_dir`'s parent and refuses if a `CURRENT` is there at all —
/// readable or not, naming this prefix or another. That is the opposite reading of `CURRENT` from
/// [`reclaim_prefix`]'s and it is why the two are separate functions: one deletes what a live
/// pointer says is superseded, the other deletes what no pointer has ever reached. Neither will
/// delete a prefix on the other's evidence.
///
/// # Errors
///
/// - [`StoreError::ReclaimRefusedCurrentExists`] if the bundle root has a `CURRENT` — a typed
///   refusal, because this is a delete and a caller must be able to tell that nothing was deleted.
/// - [`StoreError::MalformedBundle`] if `prefix_dir` has no file name or no parent.
/// - [`StoreError::Io`] if the delete itself fails.
pub fn reclaim_unpublished_prefix(prefix_dir: &Path) -> Result<()> {
    let (prefix_name, bundle_root) = prefix_parts(prefix_dir, "reclaim_unpublished_prefix")?;
    let current_path = bundle_root.join("CURRENT");
    // `try_exists` and not `exists`: a `CURRENT` this process cannot stat is a `CURRENT` this
    // function cannot prove is absent, and the refusal is the same either way.
    if current_path.try_exists().unwrap_or(true) {
        return Err(StoreError::ReclaimRefusedCurrentExists {
            prefix: prefix_name.to_string(),
            current: current_path,
        });
    }
    remove_tree(prefix_dir)
}

/// A prefix directory's own name and the bundle root holding it.
fn prefix_parts<'a>(prefix_dir: &'a Path, what: &str) -> Result<(&'a str, &'a Path)> {
    let prefix_name = prefix_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| StoreError::MalformedBundle {
            detail: format!(
                "{what}: {} has no prefix directory name to compare against CURRENT",
                prefix_dir.display()
            ),
        })?;
    let bundle_root = prefix_dir
        .parent()
        .ok_or_else(|| StoreError::MalformedBundle {
            detail: format!(
                "{what}: {} has no parent directory to read CURRENT from",
                prefix_dir.display()
            ),
        })?;
    Ok((prefix_name, bundle_root))
}

/// The one removal both reclaims run, once each has made its own proof.
fn remove_tree(prefix_dir: &Path) -> Result<()> {
    std::fs::remove_dir_all(prefix_dir).map_err(|source| StoreError::Io {
        path: prefix_dir.to_path_buf(),
        source,
    })
}
