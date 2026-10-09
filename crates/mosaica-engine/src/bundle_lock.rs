//! The write lock on a bundle root: one executor owns a bundle, and a second refuses to start.
//!
//! A side-manifest number, an entity id and a `seg_id` are all allocated from state one executor
//! holds in memory, and each is a name a second writer over the same root can take too. The
//! side-manifest allocator raises its floor over the files on disc (`write-path.md` §1.2) so that a
//! second writer cannot make the node livelock, and that is a floor under the damage rather than a
//! defence: two executors publishing complete current state for one partition still overwrite each
//! other's `MANIFEST.json` and rebase on manifests the other is replacing. The lock is what makes
//! the second writer impossible.
//!
//! **`flock`, not `fcntl` locks**: `fcntl` (POSIX record) locks are held per process, so two
//! executors inside one process — which is what a restart that leaves its predecessor running
//! produces, and how this was found — would each be granted the lock. `flock` is held per open file
//! description, so a second `open` plus `flock` in the same process conflicts.
//!
//! **The bundle root directory, not `CURRENT` and not the WAL.** `CURRENT` is replaced by a rename
//! at every fold, so a lock taken on it is held on an inode the next reader never opens. The WAL
//! path is per node, so two nodes configured with different WALs over one bundle would both be
//! granted it, which is the case this exists to refuse. The directory is the bundle's identity and
//! nothing renames it.
//!
//! **Same host only.** `flock` over SMB and NFS is unreliable, and a bundle is never served from a
//! share (`docs/evidence` on the staging tier), so the guarantee is stated as what it is: two
//! processes on one machine cannot both write one bundle root.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// An exclusive `flock` on a bundle root, released when this value is dropped.
#[derive(Debug)]
pub(crate) struct BundleWriteLock {
    /// Held open for the lock's life: `flock` is released by the last close of this description.
    _dir: File,
    path: PathBuf,
}

/// Why a bundle root could not be locked. Public — re-exported at the crate root — because it is
/// what `ExecutorStartError::BundleLocked` carries to an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleLockError {
    /// Another open file description holds the lock. `holder` is the pid `/proc/locks` names for
    /// it, absent where that file could not be read or carried no matching entry.
    Held {
        path: PathBuf,
        holder: Option<u32>,
    },
    /// The directory could not be opened or locked for any other reason — a filesystem that does
    /// not implement `flock` among them, which refuses every write to that bundle at start. The
    /// error is carried formatted, because what an operator needs here is the kernel's sentence and
    /// not a variant name.
    Io { path: PathBuf, detail: String },
}

impl std::fmt::Display for BundleLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleLockError::Held { path, holder } => {
                write!(f, "another writer holds {}", path.display())?;
                match holder {
                    Some(pid) => write!(f, " (process {pid})"),
                    None => Ok(()),
                }
            }
            BundleLockError::Io { path, detail } => {
                write!(f, "{} could not be locked ({detail})", path.display())
            }
        }
    }
}

impl BundleWriteLock {
    /// Take the exclusive lock on `bundle_root`, or say who holds it.
    ///
    /// Non-blocking: a writer that waited would sit behind a process that may run for days, and the
    /// operator needs the refusal rather than the queue.
    pub(crate) fn acquire(bundle_root: &Path) -> Result<Self, BundleLockError> {
        let dir = File::open(bundle_root).map_err(|e| BundleLockError::Io {
            path: bundle_root.to_path_buf(),
            detail: e.to_string(),
        })?;
        // SAFETY: `dir` owns the descriptor and outlives the call.
        let locked = unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let error = std::io::Error::last_os_error();
            return Err(match error.kind() {
                std::io::ErrorKind::WouldBlock => BundleLockError::Held {
                    path: bundle_root.to_path_buf(),
                    holder: holder_pid(&dir),
                },
                _ => BundleLockError::Io {
                    path: bundle_root.to_path_buf(),
                    detail: error.to_string(),
                },
            });
        }
        Ok(Self {
            _dir: dir,
            path: bundle_root.to_path_buf(),
        })
    }

    /// The root this lock is held on, for a log line that has to say which bundle.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// The pid `/proc/locks` names as holding a `FLOCK` on this file's inode, where it can be read.
///
/// Best effort by construction: `flock` itself reports nothing about the holder, `/proc/locks` is
/// Linux-only, and the entry can be gone by the time it is read. An absent answer costs the
/// operator a pid in one error message, so nothing here fails a caller.
fn holder_pid(dir: &File) -> Option<u32> {
    let inode = dir.metadata().ok()?.ino();
    let locks = std::fs::read_to_string("/proc/locks").ok()?;
    for line in locks.lines() {
        // `1: FLOCK  ADVISORY  WRITE 1234 08:02:5678 0 EOF`
        let mut fields = line.split_whitespace().skip(1);
        if fields.next() != Some("FLOCK") {
            continue;
        }
        let mut rest = fields.skip(2);
        let (Some(pid), Some(position)) = (rest.next(), rest.next()) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        // MAJOR:MINOR:INODE, the first two in hex. The inode alone identifies the file for this
        // purpose: a bundle root and an unrelated file with the same inode on another device is a
        // wrong pid in a message, not a wrong decision.
        if position
            .rsplit(':')
            .next()
            .and_then(|i| i.parse::<u64>().ok())
            == Some(inode)
        {
            return Some(pid);
        }
    }
    None
}
