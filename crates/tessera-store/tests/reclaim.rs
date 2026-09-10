//! `hard_link_forward` and the two reclaims (compaction §8): the fold's carry-forward and
//! whole-prefix delete. Each test's doc names the mutation it kills.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tessera_store::manifest::CurrentPointer;
use tessera_store::{hard_link_forward, reclaim_prefix, reclaim_unpublished_prefix, StoreError};

fn write_current(root: &Path, prefix: &str) {
    let current = CurrentPointer {
        prefix: prefix.to_string(),
        // reclaim_prefix never digest-checks MANIFEST.json against this — it only compares
        // `prefix` — so a placeholder digest is fine here.
        manifest_digest: "0".repeat(64),
    };
    fs::write(
        root.join("CURRENT"),
        serde_json::to_vec_pretty(&current).expect("serialise CURRENT"),
    )
    .expect("write CURRENT");
}

/// Carried-forward files survive the old prefix's deletion, **asserted by inode**.
///
/// This is compaction §8's stated property ("deleting the old tree unlinks directory entries
/// and never live data") and the epic's "Done when — the old prefix is reclaimed whole, asserted
/// by inode". Asserting only that the new prefix's file still exists and reads back correctly is
/// not enough: an implementation that copied instead of linking would pass that check too, and
/// would have silently doubled the disc the fold already doubles. Comparing `ino()` before and
/// after is what kills a `hard_link_forward` that copies (`fs::copy`) instead of linking
/// (`fs::hard_link`) — a mutation the weaker "still exists" assertion cannot see.
///
/// The relative path is nested (`dictionary/terms-0.dict`) to exercise the "creating parent
/// directories as needed" half of the contract in the same test.
#[test]
fn carried_forward_file_survives_old_prefix_deletion_same_inode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let old_prefix = root.join("v00000");
    let new_prefix = root.join("v00001");
    fs::create_dir_all(old_prefix.join("dictionary")).expect("mkdir old dictionary");
    fs::create_dir_all(&new_prefix).expect("mkdir new prefix");

    let rel = "dictionary/terms-0.dict".to_string();
    fs::write(old_prefix.join(&rel), b"carried verbatim, never renumbered")
        .expect("write dict extent");

    let old_ino = fs::metadata(old_prefix.join(&rel))
        .expect("stat old file")
        .ino();

    hard_link_forward(&old_prefix, &new_prefix, std::slice::from_ref(&rel))
        .expect("hard_link_forward");

    let linked_ino = fs::metadata(new_prefix.join(&rel))
        .expect("stat linked file before reclaim")
        .ino();
    assert_eq!(
        linked_ino, old_ino,
        "hard_link_forward must add a directory entry for the same inode, not a copy"
    );

    // CURRENT names the new prefix — the ordinary post-flip, post-rotation state in which
    // reclamation runs (compaction §1's diagram: swap, then WAL rotation, then reclaim).
    write_current(root, "v00001");

    reclaim_prefix(&old_prefix).expect("reclaim_prefix");

    assert!(!old_prefix.exists(), "the old prefix tree must be gone");

    let survived = fs::read(new_prefix.join(&rel)).expect("carried-forward file must still read");
    assert_eq!(survived, b"carried verbatim, never renumbered");

    let survived_ino = fs::metadata(new_prefix.join(&rel))
        .expect("stat linked file after reclaim")
        .ino();
    assert_eq!(
        survived_ino, old_ino,
        "the surviving file must be the same inode the old prefix's copy was, before *and* \
         after the old tree is deleted — proving the delete only removed the old directory \
         entry"
    );
}

/// Reclaiming a prefix that `CURRENT` still names is refused.
///
/// A wrong `prefix_dir` here is unrecoverable data loss, so this is the one check standing
/// between a caller bug (reclaiming the live prefix instead of the retired one) and deleting a
/// bundle a request may still be serving from. Kills a `reclaim_prefix` that deletes
/// unconditionally, or that reads `CURRENT` but ignores the comparison.
#[test]
fn reclaiming_the_current_prefix_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let live_prefix = root.join("v00000");
    fs::create_dir_all(&live_prefix).expect("mkdir live prefix");
    fs::write(live_prefix.join("MANIFEST.json"), b"{}").expect("write MANIFEST.json");
    write_current(root, "v00000");

    let err = reclaim_prefix(&live_prefix).expect_err("must refuse to reclaim the live prefix");
    // **Matched on the variant, not on free text.** This is the only operation in the system that
    // deletes bundle data, so "it refused, and nothing was deleted" has to be a fact a caller can
    // read off the type — a `contains("live")` assertion goes quietly green the day someone
    // rewords the message and stays green if the refusal is replaced by a different error entirely.
    assert!(
        matches!(&err, StoreError::ReclaimRefused { prefix, current }
                 if prefix == "v00000" && current == "v00000"),
        "expected ReclaimRefused naming the prefix, got {err:?}"
    );

    assert!(
        live_prefix.join("MANIFEST.json").exists(),
        "the live prefix must be untouched after a refused reclaim"
    );
}

/// An unsafe `rels` entry is a typed error, not a link outside the root.
///
/// `rels` are `files`-map keys — digest-verified for content, never for the paths inside them
/// (see `reclaim::safe_join`'s doc). A `../` entry must be caught before any filesystem call
/// kills a `hard_link_forward` that joins `rels` onto the prefix with a raw `Path::join` or a
/// string `replace`, either of which would let `../escaped.txt` land outside both prefixes.
#[test]
fn an_unsafe_rels_entry_is_a_typed_error_not_a_link_outside_the_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let old_prefix = root.join("v00000");
    let new_prefix = root.join("v00001");
    fs::create_dir_all(&old_prefix).expect("mkdir old prefix");
    fs::create_dir_all(&new_prefix).expect("mkdir new prefix");

    // Would name `root/escaped.txt` — one level above both prefixes — if the traversal were
    // followed instead of refused.
    let escape_target = root.join("escaped.txt");
    fs::write(root.join("decoy.txt"), b"not a files-map entry").expect("write decoy");

    let rels = vec!["../decoy.txt".to_string()];
    let err = hard_link_forward(&old_prefix, &new_prefix, &rels)
        .expect_err("a `..` component must be refused");
    assert!(
        matches!(err, StoreError::UnsafePath { .. }),
        "expected UnsafePath, got {err:?}"
    );

    assert!(
        !escape_target.exists(),
        "no link may be created outside either prefix"
    );

    // Same for an absolute path and a backslash-bearing one — the other two shapes `safe_join`
    // rejects.
    for unsafe_rel in ["/etc/passwd", "a\\b"] {
        let err = hard_link_forward(&old_prefix, &new_prefix, &[unsafe_rel.to_string()])
            .expect_err(&format!("'{unsafe_rel}' must be refused"));
        assert!(
            matches!(err, StoreError::UnsafePath { .. }),
            "expected UnsafePath for '{unsafe_rel}', got {err:?}"
        );
    }
}

/// Linking onto an existing target is an error rather than a silent overwrite.
///
/// The target existing means two passes wrote the same path — a caller bug (contracts §2.1: a
/// `seg_id` is never reused for exactly this reason). Kills a `hard_link_forward` that ignores
/// `AlreadyExists`, or one that removes/truncates the existing target before linking, either of
/// which would silently discard whichever file got there first.
#[test]
fn linking_onto_an_existing_target_is_an_error_not_a_silent_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let old_prefix = root.join("v00000");
    let new_prefix = root.join("v00001");
    fs::create_dir_all(&old_prefix).expect("mkdir old prefix");
    fs::create_dir_all(&new_prefix).expect("mkdir new prefix");

    let rel = "terms/postings.arrow".to_string();
    fs::create_dir_all(old_prefix.join("terms")).expect("mkdir old terms");
    fs::write(old_prefix.join(&rel), b"source").expect("write source");

    fs::create_dir_all(new_prefix.join("terms")).expect("mkdir new terms");
    fs::write(new_prefix.join(&rel), b"already here from a first pass")
        .expect("write pre-existing target");

    let err = hard_link_forward(&old_prefix, &new_prefix, std::slice::from_ref(&rel))
        .expect_err("must refuse to overwrite an existing target");
    match err {
        StoreError::MalformedBundle { detail } => {
            assert!(
                detail.contains("already exists"),
                "error should say the target already exists: {detail}"
            );
        }
        other => panic!("expected MalformedBundle, got {other:?}"),
    }

    let untouched = fs::read(new_prefix.join(&rel)).expect("target must still read");
    assert_eq!(
        untouched, b"already here from a first pass",
        "the pre-existing target must be byte-for-byte untouched, not overwritten or truncated"
    );
}

/// **A prefix in a root with no `CURRENT` is reclaimed**, which is the partial bundle a failed
/// build leaves: it wrote its prefix and never reached the pointer, so nothing names it.
///
/// Kills a `reclaim_unpublished_prefix` that refuses whenever `CURRENT` cannot be read — which is
/// what `reclaim_prefix` does, and is why the two are separate functions.
#[test]
fn an_unpublished_prefix_in_a_root_with_no_current_is_reclaimed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let prefix = root.join("v00000");
    fs::create_dir_all(prefix.join("dictionary")).expect("mkdir prefix");
    fs::write(prefix.join("dictionary/terms-0.dict"), b"partial").expect("write a partial file");

    reclaim_unpublished_prefix(&prefix).expect("no CURRENT, so nothing names this prefix");
    assert!(!prefix.exists(), "the partial prefix must be gone");
    assert!(root.exists(), "the bundle root itself is not this function's business");
}

/// **A `CURRENT` of any kind refuses the unpublished reclaim**, whether it names this prefix or
/// another, because absence is that function's whole proof.
///
/// Kills a `reclaim_unpublished_prefix` that reads `CURRENT` and compares prefix names — that
/// would delete a superseded prefix on the wrong evidence, and `reclaim_prefix` is the entry point
/// that carries the right one.
#[test]
fn an_unpublished_reclaim_refuses_wherever_a_current_exists() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let prefix = root.join("v00000");
    fs::create_dir_all(&prefix).expect("mkdir prefix");
    fs::write(prefix.join("MANIFEST.json"), b"{}").expect("write MANIFEST.json");
    // A pointer naming a *different* prefix: `reclaim_prefix` would delete this one.
    write_current(root, "v00001");

    let err = reclaim_unpublished_prefix(&prefix).expect_err("a CURRENT exists");
    assert!(
        matches!(&err, StoreError::ReclaimRefusedCurrentExists { prefix: p, .. } if p == "v00000"),
        "expected ReclaimRefusedCurrentExists naming the prefix, got {err:?}"
    );
    assert!(
        prefix.join("MANIFEST.json").exists(),
        "nothing may be deleted on a refusal"
    );
}
