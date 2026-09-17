//! **The build's term images**: one view's authorisation terms projected into its row space and
//! written beside the segment that defines it (`tessera_store::term_images` for the file and the
//! keep rule).
//!
//! # One implementation, called from both build routes
//!
//! The streaming pipeline and the in-memory build both call [`run`], and
//! `tests/build_equivalence.rs` holds the two to one bundle, so a term-image file is the same
//! bytes whichever route produced it. The derivation itself is the store's
//! [`tessera_store::term_images::derive_term_images`], which the fold calls as well (decisions
//! 0091 and 0139): nothing here projects a posting.
//!
//! # Where it runs
//!
//! After the view's segment and its `permutation.bin` are written and fsynced, and before the
//! manifests. The images are a function of the permutation, so they cannot be derived earlier; the
//! file has to be in `artifact_paths` before the digest pass, so it cannot be derived later.
//!
//! # A failure here fails the build
//!
//! Unlike the artifact pass, whose structures a request composes for itself when one is missing,
//! this reports no partial success. The manifest entry and the file are written by the same pass,
//! and a bundle naming a file that is absent is refused at open. A build that could not write the
//! images stops instead, on the rule a failed permutation write follows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tessera_authz::postings::{PostingRef, PostingsReader};
use tessera_store::derived::{DerivedIndex, PostingSlice};
use tessera_store::manifest::{TermImageExtent, DECLARED_INCARNATION};
use tessera_store::term_images::{
    derive_term_images, DeriveOptions, TermImageStamp, KEEP_ROWS_PER_CONTAINER,
};
use tessera_store::{Permutation, RowSpace};

use crate::error::{BuildError, Result};

/// **Zero, because a build is publication zero**: the file is named after the manifest generation
/// it belongs to, and a build writes `SEGMENTS-0.json`.
const MANIFEST_N: u64 = 0;

/// What one view's derivation came to, for [`crate::ViewReport`].
///
/// Reported per view rather than per bundle because a group's keys are separate views over one
/// dictionary (ruling G, `docs/evidence/memos/2026-09-14-term-images.md`): each pays its own table
/// and its own payload, and the sum says nothing about which view is expensive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TermImageReport {
    /// Terms in the dictionary, which is the number of table entries.
    pub terms: u32,
    /// Terms whose image was kept.
    pub kept: u32,
    /// Terms whose posting was too small to pass the keep rule, so was not projected.
    pub skipped_small: u32,
    /// Payload bytes, including the padding between images.
    pub payload_bytes: u64,
    /// Table bytes.
    pub table_bytes: u64,
    /// The largest single image.
    pub largest_image_bytes: u64,
    /// Seconds spent deriving, writing and syncing.
    pub wall_s: f64,
}

/// One view's file, its manifest entry and its figures.
pub struct ViewTermImages {
    /// The entry for `SEGMENTS-0.json`'s `term_image_extents`.
    pub extent: TermImageExtent,
    /// The file, for the digest pass that fills `MANIFEST.files`.
    pub path: PathBuf,
    pub report: TermImageReport,
}

/// Derive `view`'s term images into `prefix_dir`, or return `None` where there are none to derive.
///
/// `postings` is the prefix's `terms/postings.arrow`, opened once for the build: it is entity
/// space and every view projects the same postings through its own permutation.
///
/// A view with no rows and a dictionary with no terms both give `None`, and no file and no
/// manifest entry. Neither has an image to hold: projection maps entities to rows, and a view that
/// holds no row projects every posting to the empty set.
pub fn run(
    postings: &PostingsReader,
    prefix_dir: &Path,
    partition: &str,
    view: &str,
    rows_in_view: u32,
    index: &mut DerivedIndex,
) -> Result<Option<ViewTermImages>> {
    let dict_len = postings.term_count();
    if rows_in_view == 0 || dict_len == 0 {
        return Ok(None);
    }

    // **The row space of the bundle this build just wrote**, reloaded from the file rather than
    // kept from the sort, so the images are a function of the published permutation. The artifact
    // pass loads it for the same reason and the two loads are independent: it may return before
    // reaching one, and a view with no drawn layer never reaches its.
    let permutation_path =
        tessera_store::view_path(&prefix_dir.join("partitions").join(partition), view)
            .join("permutation.bin");
    let permutation = Permutation::load(&permutation_path)?;
    let space = RowSpace::new(Arc::new(permutation), rows_in_view);

    let stamp = TermImageStamp {
        prefix: crate::PREFIX.to_string(),
        view: view.to_string(),
        base_seg_id: crate::SEG_ID.to_string(),
        // **The declared incarnation** (decision 0115): a build coins each key once.
        incarnation: DECLARED_INCARNATION,
        base_rows: rows_in_view,
        bound: space.base().bound(),
    };

    let file = tessera_store::derived::term_image_file(prefix_dir, partition, MANIFEST_N, index)
        .map_err(|e| BuildError::io(prefix_dir, e))?;

    // The one adapter between the postings format and the derivation: `tessera-store` does not
    // depend on `tessera-authz`, so the shape is handed across and the walk is written where the
    // format lives. `artifact_pass::containment` holds the identical six lines.
    let walk = |term: u32, visit: &mut dyn FnMut(PostingSlice<'_>)| -> std::io::Result<()> {
        if let Some(posting) = postings.posting_at(term)? {
            match posting {
                PostingRef::Array(bytes) => visit(PostingSlice::Array(bytes)),
                PostingRef::Roaring(bitmap) => visit(PostingSlice::Roaring(&bitmap)),
            }
        }
        Ok(())
    };
    let options = DeriveOptions {
        threads: rayon::current_num_threads().max(1),
    };
    let summary = derive_term_images(&space, dict_len, &walk, &stamp, &file.path, options)
        .map_err(|e| BuildError::io(&file.path, e))?;
    // The derivation syncs the file. The directory entry has to be durable too, or a crash leaves
    // a manifest naming a file whose name was never written.
    crate::fsync_dir(&file.dir)?;

    let report = TermImageReport {
        terms: summary.terms,
        kept: summary.kept,
        skipped_small: summary.skipped_small,
        payload_bytes: summary.payload_bytes,
        table_bytes: summary.table_bytes,
        largest_image_bytes: summary.largest_image_bytes,
        wall_s: summary.wall.as_secs_f64(),
    };
    eprintln!(
        "  view '{view}': term images: {} kept of {} terms, {} payload, {} table, largest {}, \
         {:.1} s",
        crate::thousands(u64::from(report.kept)),
        crate::thousands(u64::from(report.terms)),
        crate::unique_key::human_bytes(report.payload_bytes),
        crate::unique_key::human_bytes(report.table_bytes),
        crate::unique_key::human_bytes(report.largest_image_bytes),
        report.wall_s,
    );

    Ok(Some(ViewTermImages {
        extent: TermImageExtent {
            path: file.rel,
            view: view.to_string(),
            incarnation: DECLARED_INCARNATION,
            dict_len,
            keep_rows_per_container: KEEP_ROWS_PER_CONTAINER as u32,
        },
        path: file.path,
        report,
    }))
}
