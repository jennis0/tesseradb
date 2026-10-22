//! Hand-assemble a tiny bundle in a tempdir (real digests, computed with
//! `sha2`, the same crate the reader uses) and exercise the full read protocol end to end:
//! `open_bundle`, `ColumnsRef` accessors, `Permutation::project` against a per-entity
//! `row_of` loop, and `tile_ranges` against a linear scan of the morton array. Then corrupt
//! one byte of `columns.arrow` and confirm `open_bundle` fails closed with a digest error.

use std::fs;
use std::path::Path;

use croaring::Bitmap;

use mosaica_spatial::{split32, tiles_for_bbox, tiles_for_bbox_count, Bounds, Tile};
use mosaica_store::manifest::{CurrentPointer, EntitySet};
use mosaica_store::{open_bundle, tile_ranges, StoreError};
use mosaica_types::EntityId;

mod fixture;
use fixture::{
    add_shard_one, build_bundle, file_digest, hex_sha256, read_assembled, shard_file,
    write_assembled, write_assembled_named, SHARD,
};

fn seg_file(seg_id: &str, file: &str) -> String {
    format!("{}/{file}", SHARD.segment_rel("main", seg_id))
}

fn partition_dir(root: &Path) -> std::path::PathBuf {
    mosaica_store::layout::partition_dir(&root.join("v00000"), "default")
}

fn unit_extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

#[test]
fn open_bundle_loads_segments_and_columns_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (items, codes) = build_bundle(dir.path(), 200);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    assert_eq!(bundle.manifest.bundle_format, mosaica_types::BUNDLE_FORMAT);

    let partition = bundle.partitions.get("default").expect("default partition");
    assert_eq!(partition.segments_n, 0);

    let view = partition.only_shard().views.get("main").expect("main view");
    assert_eq!(view.segments.len(), 1);
    let seg = &view.segments[0];
    assert_eq!(seg.seg_id, "seg0");
    assert_eq!(seg.row_count, items.len() as u32);

    // columns.arrow round-trips row 0..n exactly. (No `priority` column — decision 0046; the
    // quantity is the high 16 bits of `mosaica_id`, carried in the same row.)
    let mosaica_id_col = seg.columns.mosaica_id();
    let residual_col = seg.columns.residual();
    assert_eq!(mosaica_id_col.len(), items.len());
    for (i, item) in items.iter().enumerate() {
        assert_eq!(mosaica_id_col[i], item.mosaica_id.raw());
        assert_eq!(residual_col[i], split32(item.qx, item.qy).1);
    }

    // morton.u32 round-trips exactly what sort_batch computed, and the two files' words
    // concatenate back to the position the item went in with — which is the whole point of
    // splitting them across two files.
    assert_eq!(seg.morton.u32(), codes.as_slice());
    for (i, item) in items.iter().enumerate() {
        let (cell, residual) = split32(item.qx, item.qy);
        assert_eq!(
            ((seg.morton.u32()[i] as u64) << 32) | residual_col[i] as u64,
            ((cell.raw() as u64) << 32) | residual as u64
        );
    }
}

#[test]
fn permutation_project_matches_per_entity_row_of_loop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (items, _codes) = build_bundle(dir.path(), 500);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let view = &bundle.only_shard().views["main"];

    // A mask over roughly half the entity space, including a few IDs outside [0, n) — those
    // must be silently skipped (no row here), not treated as an error.
    let mut mask = Bitmap::new();
    for e in (0..items.len() as u32).step_by(3) {
        mask.add(e);
    }
    mask.add(10_000); // out of bound: must be dropped, not panic

    let projected = view.row_space.project(&mask);

    // Independently recompute the expected row set via `row_of`, one entity at a time.
    let mut expected_rows: Vec<u32> = Vec::new();
    for e in mask.iter() {
        if let Some(row) = view.row_space.row_of(EntityId::new(e as u64)) {
            expected_rows.push(row.raw());
        }
    }
    expected_rows.sort_unstable();

    let projected_rows: Vec<u32> = projected.iter().collect();
    assert_eq!(projected_rows, expected_rows);
    assert!(
        !projected_rows.is_empty(),
        "sanity: the mask should have produced at least one row"
    );
}

#[test]
fn tile_ranges_agrees_with_linear_scan_of_morton_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, codes) = build_bundle(dir.path(), 300);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];

    // A handful of tiles at different depths, including the whole-grid tile (depth 0).
    for tile in [
        Tile {
            prefix: 0,
            depth: 0,
        },
        Tile {
            prefix: 0,
            depth: 2,
        },
        Tile {
            prefix: 1,
            depth: 2,
        },
        Tile {
            prefix: 3,
            depth: 2,
        },
        Tile {
            prefix: (1u64 << 8) - 1,
            depth: 4,
        },
    ] {
        let (lo, hi) = tile.code_range();
        let expected_start = codes
            .iter()
            .position(|&c| c as u64 >= lo)
            .unwrap_or(codes.len());
        let expected_end = codes
            .iter()
            .position(|&c| c as u64 >= hi)
            .unwrap_or(codes.len());

        let range = tile_ranges(seg, &tile);
        assert_eq!(range.start as usize, expected_start, "tile {tile:?} start");
        assert_eq!(range.end as usize, expected_end, "tile {tile:?} end");
    }
}

#[test]
fn tile_ranges_covers_the_whole_segment_at_depth_zero() {
    // Depth 0's exclusive code-range end is `1 << 32`, which does not fit in u32. This test
    // exists because narrowing `Tile::code_range` alongside the stored column would overflow
    // here and silently return an empty range in release builds — the codes are widened for
    // the comparison instead (contracts §2.5, r5).
    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, codes) = build_bundle(dir.path(), 300);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];

    let whole = tile_ranges(
        seg,
        &Tile {
            prefix: 0,
            depth: 0,
        },
    );
    assert_eq!(
        whole,
        0u32..codes.len() as u32,
        "depth 0 must select every row in the segment"
    );
}

/// Read publication 0 under `root`'s bundle prefix as one JSON value, apply `edit`, and write it
/// back in place, every shard and space file with its digest named again — the standard way these
/// tests attack the manifest layer without touching the real segment files or breaking the
/// `MANIFEST.json` digest chain.
fn edit_segments_manifest(root: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let prefix_dir = root.join("v00000");
    let mut value = read_assembled(&prefix_dir, 0);
    edit(&mut value);
    write_assembled(&prefix_dir, 0, value);
}

/// Publication 0 written again as publication `n`, with `edit` applied to its JSON first.
///
/// Unless `edit` says otherwise, every data file the copy names is untouched on disk, so the copy
/// verifies by size and digest exactly as the original does. That is the point: with
/// verification held constant, a step-down (or a refusal to step down) in these fixtures can
/// only have come from the honourable-state check, never from a file that failed to verify.
///
/// Holding it constant is also what makes the *ordering* of the two checks unobservable, which
/// is why one test (`a_deny_carrying_manifest_whose_files_are_missing_is_still_refused`) breaks
/// the pattern deliberately and has `edit` name a file that does not exist.
fn add_segments_manifest(root: &Path, n: u64, edit: impl FnOnce(&mut serde_json::Value)) {
    let prefix_dir = root.join("v00000");
    let mut value = read_assembled(&prefix_dir, 0);
    edit(&mut value);
    write_assembled(&prefix_dir, n, value);
}

// -------------------------------------------------------------------------------------------
// Honoured deny state, and the refusal to step past it (contracts §2.3, SA §9).
//
// `deltas`, `deny` and `tombstones` are all honoured now: a fragment build unions every live
// tier, and the loader seeds the initial overlay from `deny`/`tombstones`. So a manifest
// carrying any of them OPENS — which moves the whole guard from classification to verification.
//
// The rule that remains, and that these tests pin, is the one that matters:
//
//   * a candidate whose files do not verify and which carries NO deny-disposition state is
//     stepped past — missing items, staleness in the fail-safe direction, the mid-sync replica's
//     availability argument;
//   * a candidate whose files do not verify and which DOES carry deny-disposition state is
//     UNREADY. Stepping past it serves an older manifest in which every entity denied since is
//     visible again — indefinitely, since the freshness gate §2.3 pairs with step-down does not
//     exist.
//
// Each test asserts the *disposition*, not merely "an error happened": a guard that refused
// everything would pass a test that only checked for `Err`.
//
// The fixtures withhold a file rather than corrupting a shared one. Corrupting `columns.arrow`
// fails SEGMENTS-0's verification too, so a stepping-down reader would merely return a different
// error instead of *serving* — and serving is the failure being guarded against.
// -------------------------------------------------------------------------------------------

fn deny_set(ids: &[u32]) -> EntitySet {
    EntitySet::of(&ids.iter().copied().collect::<croaring::Bitmap>())
}

fn shard0(value: &mut serde_json::Value) -> &mut serde_json::Value {
    &mut value["shards"]["records"][0]
}

/// Name a file in this manifest's `files` map that does not exist on disk, so verification fails
/// for this candidate and no other. Any digest will do: the file cannot be opened, so it never
/// reaches the comparison.
fn name_a_missing_file(value: &mut serde_json::Value) {
    value["files"][seg_file("seg-unsynced", "columns.arrow")] =
        serde_json::json!({ "size": 4, "sha256": "00".repeat(32) });
}

#[test]
fn a_manifest_carrying_tombstones_opens_and_carries_them_forward() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    edit_segments_manifest(dir.path(), |value| {
        shard0(value)["tombstones"] = serde_json::to_value(deny_set(&[17])).unwrap();
    });

    let bundle = open_bundle(dir.path()).expect("tombstones are honoured, so this opens");
    assert_eq!(
        bundle.only_shard().record.tombstones.entities().unwrap().to_vec(),
        vec![17],
        "the loader must carry the deletion to whoever seeds the overlay from it — an opened \
         manifest whose tombstones went nowhere serves entity 17"
    );
}

#[test]
fn a_manifest_carrying_a_deny_entry_opens_and_carries_it_forward() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    edit_segments_manifest(dir.path(), |value| {
        shard0(value)["deny"] = serde_json::to_value(deny_set(&[17])).unwrap();
    });

    let bundle = open_bundle(dir.path()).expect("deny is honoured, so this opens");
    let deny = &bundle.only_shard().record.deny;
    assert_eq!(deny.entities().unwrap().to_vec(), vec![17]);
}

/// **The fail-open this task exists to close, and the one the honouring change re-opened if the
/// two halves had been separated.**
///
/// With `deny` honoured the classification arm no longer objects to this manifest at all: it is
/// `Honourable`, goes to `verify_files`, and a `continue` there steps down to a SEGMENTS-0 that
/// verifies perfectly and in which entity 17 is visible again. Not an error of the wrong type —
/// an `Ok` that serves the pre-suppression state.
///
/// The fixture is the mid-sync replica this is really about: the newest manifest carries an
/// accepted suppression and names a segment file that has not arrived yet.
#[test]
fn a_deny_carrying_candidate_that_fails_verification_is_unready_never_stepped_past() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    add_segments_manifest(dir.path(), 1, |value| {
        shard0(value)["deny"] = serde_json::to_value(deny_set(&[17])).unwrap();
        name_a_missing_file(value);
    });

    let err = open_bundle(dir.path()).expect_err(
        "a deny-carrying candidate whose files do not verify must make the partition unready",
    );
    match err {
        StoreError::UnverifiedDenyManifest { n, fields, .. } => {
            assert_eq!(
                n, 1,
                "the refusal must name the candidate that carried the deny"
            );
            assert!(fields.contains(&"deny"), "got: {fields:?}");
        }
        other => panic!(
            "expected UnverifiedDenyManifest — stepping down to SEGMENTS-0 here re-exposes an \
             accepted suppression. Got: {other}"
        ),
    }
}

/// **Bytes that do not decode are not an empty set.** A `deny` field is one base64 string holding
/// a portable Roaring bitmap; a damaged one says nothing about how many entities it named, so the
/// partition is unready. Reading it as empty would serve every entity it carried, and stepping
/// past it would serve an older manifest that predates the deny.
#[test]
fn a_deny_field_that_does_not_decode_makes_the_partition_unready() {
    for (field, damaged) in [
        ("deny", serde_json::json!("not base64 at all")),
        ("tombstones", serde_json::json!("")),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        build_bundle(dir.path(), 50);

        // On the newest manifest, whose files all verify: nothing but the undecodable field is
        // wrong with it.
        add_segments_manifest(dir.path(), 1, |value| {
            shard0(value)[field] = damaged.clone();
        });

        match open_bundle(dir.path())
            .expect_err("a deny field that does not decode must refuse, never read as empty")
        {
            StoreError::UnhonourableManifest { n, fields, .. } => {
                assert_eq!(n, 1);
                assert!(fields.contains(&field), "got: {fields:?}");
            }
            other => panic!("expected UnhonourableManifest, got: {other}"),
        }
    }
}

/// The same for a tombstone, which is the other deny-disposition field: a deletion re-exposed is
/// no better than a suppression re-exposed.
#[test]
fn a_tombstone_carrying_candidate_that_fails_verification_is_unready_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    add_segments_manifest(dir.path(), 1, |value| {
        shard0(value)["tombstones"] = serde_json::to_value(deny_set(&[17])).unwrap();
        name_a_missing_file(value);
    });

    match open_bundle(dir.path())
        .expect_err("a tombstone-carrying candidate must not be stepped past")
    {
        StoreError::UnverifiedDenyManifest { fields, .. } => {
            assert!(fields.contains(&"tombstones"), "got: {fields:?}")
        }
        other => panic!("expected UnverifiedDenyManifest, got: {other}"),
    }
}

/// **The shape §2.3 actually publishes, and the one-identifier fail-open.** A side-manifest is
/// "full current state, not a diff" and `deny` is "the current suppression set", so every
/// manifest published while any suppression is live carries `deny` **and** whatever `deltas`
/// exist — `deny` alone is the rarer case.
///
/// Over `["deny", "deltas"]` the difference between "carries deny-disposition state" and
/// "carries only steppable state" is the difference between refusing and serving the suppressed
/// entity, and `deny_disposition_state`'s filter is the one identifier that decides it.
#[test]
fn a_deny_alongside_deltas_is_not_stepped_past_either() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    add_segments_manifest(dir.path(), 1, |value| {
        shard0(value)["deny"] = serde_json::to_value(deny_set(&[17])).unwrap();
        shard0(value)["deltas"] = serde_json::json!([seg_file("flush-1-1", "delta.arrow")]);
        name_a_missing_file(value);
    });

    match open_bundle(dir.path()).expect_err(
        "a candidate carrying a deny *and* deltas is a deny-carrying candidate; stepping down to \
         SEGMENTS-0 here serves the suppressed entity",
    ) {
        StoreError::UnverifiedDenyManifest { n, fields, .. } => {
            assert_eq!(n, 1);
            assert!(fields.contains(&"deny"), "got: {fields:?}");
        }
        other => panic!(
            "expected UnverifiedDenyManifest — a deny beside a delta is still a deny. Got: {other}"
        ),
    }
}

/// The step-down's safety rests on **every** intervening candidate being examined — it is a
/// property of the loop, not of any one field. Four manifests deep, the deny two steps down the
/// walk must still stop it: SEGMENTS-3 and SEGMENTS-2 fail verification and carry no deny state
/// (so they are stepped past), SEGMENTS-1 carries a deny and fails verification, SEGMENTS-0 is
/// clean and would verify.
///
/// A reader that examined only the top candidate, or only the top few, would step past
/// SEGMENTS-1 unseen and serve SEGMENTS-0 — the same fail-open reintroduced from the other end,
/// and the shape an optimisation of `list_segments_manifests` would take.
///
/// **No fixture can catch every truncation** — for any depth there is a "top k" that still
/// reaches the deny by luck. Two steppable candidates above it catch the plausible values (take
/// the highest only; take the top two); the general prohibition is stated as a rule in
/// `load_verifying_segments_manifest`'s doc, because that is the only form it can take.
#[test]
fn the_walk_refuses_a_deny_it_reaches_only_after_stepping_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);
    add_segments_manifest(dir.path(), 1, |value| {
        shard0(value)["deny"] = serde_json::to_value(deny_set(&[17])).unwrap();
        name_a_missing_file(value);
    });
    add_segments_manifest(dir.path(), 2, name_a_missing_file);
    add_segments_manifest(dir.path(), 3, name_a_missing_file);

    match open_bundle(dir.path())
        .expect_err("the deny at n=1 must stop the walk before it reaches the clean SEGMENTS-0")
    {
        StoreError::UnverifiedDenyManifest { n, fields, .. } => {
            assert_eq!(
                n, 1,
                "the walk must reach and refuse the deny-carrying candidate, not stop at the \
                 steppable ones above it"
            );
            assert!(fields.contains(&"deny"), "got: {fields:?}");
        }
        other => panic!("expected UnverifiedDenyManifest at n=1, got: {other}"),
    }
}

/// The other half of the split, and the reason it is not a uniform refusal: a candidate carrying
/// no deny-disposition state can only be missing items — staleness in the fail-safe direction —
/// so the availability argument for a mid-sync replica holds here and only here.
#[test]
fn a_candidate_with_no_deny_state_steps_down_and_serves() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);
    add_segments_manifest(dir.path(), 1, |value| {
        shard0(value)["deltas"] = serde_json::json!([seg_file("flush-1-1", "delta.arrow")]);
        name_a_missing_file(value);
    });

    let bundle =
        open_bundle(dir.path()).expect("a candidate with no deny state may be stepped past");
    let partition = bundle.partitions.get("default").expect("default partition");
    assert_eq!(
        partition.segments_n, 0,
        "must have stepped down to SEGMENTS-0, not opened SEGMENTS-1"
    );
    assert_eq!(
        partition.only_shard().views["main"].segments[0].row_count, 50,
        "and the stepped-down state must actually serve"
    );

    // The step-down is a *success* return, so the only trace of it is a `warn!` — which nothing
    // can gate on. Contracts §2.3 pairs step-down with a `readyz` freshness gate, and this is the
    // data that gate is built from; `readyz` fails outright while it is true (§8.2).
    assert_eq!(partition.segments_n, 0, "served SEGMENTS-0");
    assert_eq!(
        partition.highest_candidate_n, 1,
        "and must still report the newest manifest it refused, or a freshness gate has nothing \
         to compare against"
    );
    assert!(partition.stepped_down());
}

/// Not "serves the oldest". When no candidate verifies the walk runs off the end of the list,
/// which is ordinary candidate exhaustion — `NoVerifyingSegmentsManifest`, never the deny
/// disposition's refusal. It must still carry the reason, so an operator is told what went wrong
/// rather than left with "nothing verified".
///
/// The reason must be the **highest** candidate's, because the newest manifest is the one an
/// operator must fix.
#[test]
fn every_candidate_failing_verification_exhausts_the_candidate_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);
    edit_segments_manifest(dir.path(), name_a_missing_file);
    add_segments_manifest(dir.path(), 1, |value| {
        value["files"][seg_file("seg-newest", "columns.arrow")] =
            serde_json::json!({ "size": 4, "sha256": "00".repeat(32) });
    });

    match open_bundle(dir.path())
        .expect_err("with no verifying candidate the partition must not open")
    {
        StoreError::NoVerifyingSegmentsManifest {
            highest_candidate_error: Some(reason),
            ..
        } => assert!(
            reason.contains("seg-newest"),
            "the reason must be the newest candidate's, not the oldest's, got: {reason}"
        ),
        other => panic!("expected NoVerifyingSegmentsManifest, got: {other}"),
    }
}

/// The negative control for all of the above: with every state field empty the guard is
/// invisible and the newest manifest opens, exactly as before this task — and, equally, no
/// step-down is reported. A guard that refused nothing would pass the tests above; a reader that
/// reported a phantom step-down would eventually fail a freshness gate on a healthy replica.
#[test]
fn a_manifest_with_no_unhonourable_state_opens_at_the_highest_n() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);
    add_segments_manifest(dir.path(), 1, |_| {});

    let bundle = open_bundle(dir.path()).expect("a clean manifest must open");
    let partition = &bundle.partitions["default"];
    assert_eq!(partition.segments_n, 1);
    assert_eq!(partition.segments_n, 1);
    assert_eq!(partition.highest_candidate_n, 1);
    assert!(
        !partition.stepped_down(),
        "nothing was refused here; reporting a step-down would make the 2.2 freshness gate lie"
    );
}

/// No shard, a shard named twice and shards out of order are each refused, and the valid
/// SEGMENTS-0 below is not served in its place.
#[test]
fn a_side_manifest_naming_no_shard_or_shards_out_of_order_is_refused() {
    let shapes: [fn(&mut serde_json::Value); 3] = [
        |records| *records = serde_json::json!([]),
        |records| {
            let second = records[0].clone();
            records.as_array_mut().unwrap().push(second);
        },
        |records| {
            let mut first = records[0].clone();
            first["id"] = serde_json::json!(1);
            records.as_array_mut().unwrap().insert(0, first);
        },
    ];
    for shape in shapes {
        let dir = tempfile::tempdir().expect("tempdir");
        build_bundle(dir.path(), 50);
        add_segments_manifest(dir.path(), 1, |value| shape(&mut value["shards"]["records"]));
        let err = open_bundle(dir.path()).expect_err("the shards are refused");
        assert!(matches!(err, StoreError::MalformedBundle { .. }), "{err:?}");
    }
}

/// **A bundle at the previous number refuses at open on the number alone** (`bundle_format` 11,
/// contracts §2.6).
///
/// A segment at 11 holds `cuts.u32` beside `morton.u32`, and a segment written at 10 does not.
/// The open names every segment file it maps, so such a bundle would refuse there in any case —
/// but it would refuse as a missing file, which is what a damaged bundle looks like. The number is
/// what says it is a stale one instead. The manifest here is exactly the one the writer at 11
/// produced with the number turned back, so nothing but the number can be what refuses.
#[test]
fn a_bundle_at_the_previous_number_is_refused_on_the_number_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 20);

    let manifest_path = dir.path().join("v00000/MANIFEST.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read MANIFEST.json"))
            .expect("parse MANIFEST.json");
    value["bundle_format"] = serde_json::json!(mosaica_types::BUNDLE_FORMAT - 1);
    let bytes = serde_json::to_vec_pretty(&value).expect("serialise");
    fs::write(&manifest_path, &bytes).expect("rewrite MANIFEST.json");
    let current = CurrentPointer {
        prefix: "v00000".to_string(),
        manifest_digest: hex_sha256(&bytes),
    };
    fs::write(
        dir.path().join("CURRENT"),
        serde_json::to_vec_pretty(&current).expect("serialise CURRENT"),
    )
    .expect("rewrite CURRENT");

    let err = open_bundle(dir.path()).expect_err("a bundle at another format must not open");
    match err {
        StoreError::UnsupportedBundleFormat { found, supported } => {
            assert_eq!(
                (found, supported),
                (
                    mosaica_types::BUNDLE_FORMAT - 1,
                    mosaica_types::BUNDLE_FORMAT
                )
            );
        }
        other => panic!("refused for the wrong reason: {other}"),
    }
}

/// **A bundle written before `views[..].projection` existed refuses at open** (`bundle_format`
/// 4, `projections.md` §3).
///
/// The frame does not imply the projection — this fixture's `[0, 1]` extent is a legal frame for
/// a view with no projection at all — so a manifest at 3 read as unprojected is not a partial
/// read but a wrong one: the write path would quantise a degree as though it were a frame
/// coordinate and nothing downstream could see that it had. `#[serde(default)]` on the field is
/// exactly that misread, which is why the key is required and why this test asserts a refusal
/// rather than a default.
#[test]
fn a_bundle_at_the_previous_format_is_refused_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 20);

    // The manifest as the writer at `bundle_format` 3 produced it: no `projection` on any view.
    let manifest_path = dir.path().join("v00000/MANIFEST.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read MANIFEST.json"))
            .expect("parse MANIFEST.json");
    value["bundle_format"] = serde_json::json!(3);
    for view in value["views"].as_array_mut().expect("views is an array") {
        view.as_object_mut()
            .expect("a view is an object")
            .remove("projection")
            .expect("the writer at 4 emitted one to remove");
    }
    let bytes = serde_json::to_vec_pretty(&value).expect("serialise");
    fs::write(&manifest_path, &bytes).expect("rewrite MANIFEST.json");
    // Re-digest, so the refusal is the format's and not `CURRENT`'s.
    let current = CurrentPointer {
        prefix: "v00000".to_string(),
        manifest_digest: hex_sha256(&bytes),
    };
    fs::write(
        dir.path().join("CURRENT"),
        serde_json::to_vec_pretty(&current).expect("serialise CURRENT"),
    )
    .expect("rewrite CURRENT");

    let err = open_bundle(dir.path())
        .expect_err("a bundle that cannot say what projected its positions must not open");
    match err {
        StoreError::Json { source, .. } => {
            let detail = source.to_string();
            assert!(
                detail.contains("projection"),
                "the refusal must name the missing key, got: {detail}"
            );
        }
        other => panic!("expected a manifest parse refusal naming `projection`, got: {other}"),
    }
}

/// **A projection name outside the set refuses the whole manifest**, rather than the one field.
///
/// A reader that cannot resolve the name cannot invert a stored position, and resolving it to
/// `none` is the same misread the field exists to stop.
#[test]
fn a_manifest_naming_a_projection_outside_the_set_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 20);

    let manifest_path = dir.path().join("v00000/MANIFEST.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read MANIFEST.json"))
            .expect("parse MANIFEST.json");
    value["views"][0]["projection"] = serde_json::json!("lambert_cylindrical_equal_area");
    let bytes = serde_json::to_vec_pretty(&value).expect("serialise");
    fs::write(&manifest_path, &bytes).expect("rewrite MANIFEST.json");
    let current = CurrentPointer {
        prefix: "v00000".to_string(),
        manifest_digest: hex_sha256(&bytes),
    };
    fs::write(
        dir.path().join("CURRENT"),
        serde_json::to_vec_pretty(&current).expect("serialise CURRENT"),
    )
    .expect("rewrite CURRENT");

    match open_bundle(dir.path()).expect_err("an unresolvable projection must not open") {
        StoreError::Json { source, .. } => {
            let detail = source.to_string();
            assert!(
                detail.contains("lambert_cylindrical_equal_area"),
                "the refusal must name what it could not resolve, got: {detail}"
            );
        }
        other => panic!("expected a manifest parse refusal, got: {other}"),
    }
}

#[test]
fn open_bundle_fails_closed_on_a_corrupted_columns_arrow_byte() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    let columns_path = dir
        .path()
        .join("v00000")
        .join(seg_file("seg0", "columns.arrow"));
    let mut bytes = fs::read(&columns_path).expect("read columns.arrow");
    // Flip a byte roughly in the middle of the file — inside the record batch body, not the
    // footer/magic, so this is a content corruption a naive length-only check would miss.
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&columns_path, &bytes).expect("rewrite corrupted columns.arrow");

    let err = open_bundle(dir.path()).expect_err("corrupted columns.arrow must fail open_bundle");
    // With only one SEGMENTS-<n>.json candidate (n=0), the outer error is always
    // `NoVerifyingSegmentsManifest` — but it must carry the *specific* reason the candidate
    // failed, not silently swallow it (the bug this replaces: `FileVerificationFailed` was
    // previously unreachable from `open_bundle`, since the step-down loop discarded every
    // per-candidate error).
    match err {
        StoreError::NoVerifyingSegmentsManifest {
            highest_candidate_error: Some(reason),
            ..
        } => {
            assert!(
                reason.contains("columns.arrow"),
                "reason should name columns.arrow, got: {reason}"
            );
            assert!(
                reason.to_ascii_lowercase().contains("sha-256")
                    || reason.to_ascii_lowercase().contains("digest"),
                "reason should describe a digest mismatch, got: {reason}"
            );
        }
        other => panic!(
            "expected NoVerifyingSegmentsManifest with a digest-mismatch reason, got: {other}"
        ),
    }
}

#[test]
fn open_bundle_rejects_a_segments_manifest_that_omits_columns_arrow_from_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 40);

    // The segment files on disk are all still byte-for-byte correct — only the SEGMENTS-0.json
    // `files` map is edited to drop the `columns.arrow` entry. `verify_files` alone would treat
    // this SEGMENTS manifest as fully verifying (there's nothing left in `files` that doesn't
    // check out) — that vacuous-verification gap is exactly what `ensure_verified` at the
    // loader call sites must close.
    edit_segments_manifest(dir.path(), |value| {
        value["files"]
            .as_object_mut()
            .expect("files is an object")
            .remove(&seg_file("seg0", "columns.arrow"));
    });

    let err = open_bundle(dir.path())
        .expect_err("a SEGMENTS manifest omitting columns.arrow from files must be rejected");
    match err {
        StoreError::UnverifiedFile { path } => {
            assert!(
                path.ends_with("columns.arrow"),
                "expected the unverified path to be columns.arrow, got: {}",
                path.display()
            );
        }
        other => panic!("expected UnverifiedFile, got: {other}"),
    }
}

/// A segment whose `bands.bin` is gone does not open: the segment's load names the band file, and
/// the bundle has no manifest left to serve.
#[test]
fn a_segment_without_its_bands_does_not_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 40);
    let seg_dir = dir
        .path()
        .join("v00000")
        .join(SHARD.segment_rel("main", "seg0"));
    fs::remove_file(seg_dir.join(mosaica_store::bands::BANDS_FILE)).unwrap();
    let refused = mosaica_store::read::SegmentData::load(
        &seg_dir,
        "seg0",
        40,
        mosaica_store::edited::RowEntities::Numbers,
    )
    .expect_err("a segment without its bands must not load");
    assert_eq!(refused.file, "bands");
    assert!(matches!(
        open_bundle(dir.path()),
        Err(StoreError::NoVerifyingSegmentsManifest { .. })
    ));
}

#[test]
fn open_bundle_rejects_a_permutation_slot_pointing_past_row_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 60);

    let perm_path = dir
        .path()
        .join("v00000")
        .join(SHARD.view_rel("main"))
        .join("permutation.bin");
    let mut bytes = fs::read(&perm_path).expect("read permutation.bin");

    // The bound is 60, so the file holds one page and the payload starts at the first 4 KiB
    // boundary past the 24-byte header and its one-entry directory
    // (`mosaica_store::permutation`). Overwrite entity 0's slot with a row index far past this
    // segment's row_count (60) — still a structurally valid `u32`, not the sentinel, just out of
    // range.
    let corrupt_slot = 9_999u32.to_le_bytes();
    bytes[4096..4100].copy_from_slice(&corrupt_slot);
    fs::write(&perm_path, &bytes).expect("rewrite corrupted permutation.bin");

    // Recompute the digest so this reaches content validation (`Permutation::validate_rows`)
    // rather than being caught earlier by the plain size+SHA-256 check — the two are different
    // defences (one catches bit-flips, the other catches internally-consistent-but-wrong data).
    let corrected = file_digest(&perm_path);
    edit_segments_manifest(dir.path(), |value| {
        let entry = &mut value["files"][format!("{}/permutation.bin", SHARD.view_rel("main"))];
        entry["size"] = serde_json::json!(corrected.size);
        entry["sha256"] = serde_json::json!(corrected.sha256);
    });

    let err =
        open_bundle(dir.path()).expect_err("a permutation slot past row_count must be rejected");
    match err {
        StoreError::InvalidPermutation { detail, .. } => {
            assert!(
                detail.contains("out of bound"),
                "expected an out-of-bound detail, got: {detail}"
            );
        }
        other => panic!("expected InvalidPermutation, got: {other}"),
    }
}

#[test]
fn open_bundle_rejects_a_path_traversing_segment_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 20);

    // The real `seg0` files are untouched; only the SEGMENTS-0.json entry that names them is
    // edited to claim a traversal-shaped seg_id. This must be rejected before any path is ever
    // joined onto the bundle root and opened — a digest-verified manifest is safe from content
    // tampering, not from carrying unsafe *values*.
    edit_segments_manifest(dir.path(), |value| {
        shard0(value)["segments"][0]["seg_id"] = serde_json::json!("../../evil");
    });

    let err =
        open_bundle(dir.path()).expect_err("a path-traversing seg_id must be rejected before use");
    match err {
        StoreError::UnsafePath { value, .. } => {
            assert_eq!(value, "../../evil");
        }
        other => panic!("expected UnsafePath, got: {other}"),
    }
}

// -------------------------------------------------------------------------------------------
// `tile_ranges_all` equivalence.
//
// `tile_ranges_all` replaces N per-tile binary searches with one monotone galloping sweep over
// the cell codes, in Morton order. Its entire safety argument is that it computes the *same
// answer* as a search of the Morton column at every index — so nothing below re-derives an
// expected range from a model of the sweep. Everything compares against `tile_ranges_within`
// over the whole segment, which searches the Morton column itself, and against `tile_ranges`.
// (The gallop primitive underneath has its own unit test against `partition_point`, in
// `mosaica_store::read`.)
//
// Two things a plausible-looking sweep gets wrong, both tested for here:
//
//   1. Returning results in Morton order. `tiles_for_bbox` enumerates in `(ty outer, tx inner)`
//      raster order, which is NOT Morton order — and the response's tile order is load-bearing
//      (see `tile_ranges_all`'s doc). Every assertion below is positional.
//   2. Assuming the tile set is shaped the way `tiles_for_bbox` happens to shape it: one depth,
//      unique, disjoint, ascending. The function is public, so the arbitrary-tile-set test
//      feeds it mixed depths, duplicates and shuffled orders.
// -------------------------------------------------------------------------------------------

/// `out[i]` is the Morton column's range for `tiles[i]`, and `tile_ranges`'s, for every `i`, or a
/// failure naming the index.
fn assert_matches_per_tile_search(seg: &mosaica_store::SegmentData, tiles: &[Tile], what: &str) {
    let swept = mosaica_store::tile_ranges_all(seg, tiles);
    assert_eq!(swept.len(), tiles.len(), "{what}: one range per tile");
    for (i, tile) in tiles.iter().enumerate() {
        let column = mosaica_store::read::tile_ranges_within(seg, tile, 0..seg.row_count);
        assert_eq!(
            swept[i], column,
            "{what}: tile {i} ({tile:?}) — the sweep must agree with the full-column search \
             AT ITS OWN INDEX"
        );
        assert_eq!(tile_ranges(seg, tile), column, "{what}: tile {i} ({tile:?})");
    }
    // `TileOrder::parts` is the same sweep, keeping only the tiles that hold a row.
    let held: Vec<(usize, std::ops::Range<u32>)> = swept
        .iter()
        .cloned()
        .enumerate()
        .filter(|(_, range)| range.start < range.end)
        .collect();
    let mut parts = mosaica_store::TileOrder::of(tiles).parts(seg);
    parts.sort_by_key(|(i, _)| *i);
    assert_eq!(parts, held, "{what}: the tiles holding a row");
}

#[test]
fn the_tiles_holding_a_row_are_found_where_most_of_a_request_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 5,000 items on a 100x100 position grid: at depth 12 nearly every tile is empty, so the sweep
    // passes over long runs of empty tiles between the occupied ones.
    build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];
    let extent = unit_extent();
    for (label, bbox) in [
        ("interior", [0.10, 0.20, 0.16, 0.26]),
        ("past the last row", [0.97, 0.97, 1.0, 1.0]),
        ("full extent", [0.0, 0.0, 1.0, 1.0]),
        ("a few cells", [0.10, 0.20, 0.102, 0.202]),
    ] {
        for depth in [7u8, 9, 12, 16] {
            if tiles_for_bbox_count(bbox, depth, &extent) > 70_000 {
                continue;
            }
            let tiles = tiles_for_bbox(bbox, depth, &extent);
            assert_matches_per_tile_search(seg, &tiles, &format!("{label} d{depth}"));
        }
    }
}

#[test]
fn a_cell_just_past_an_empty_tile_is_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, mut codes) = build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];
    codes.sort_unstable();
    codes.dedup();
    // A depth-16 tile is one cell: the empty cell below an occupied one, then the occupied one.
    for c in codes.into_iter().filter(|&c| c > 0).map(u64::from) {
        let tiles = [
            Tile {
                prefix: c - 1,
                depth: 16,
            },
            Tile {
                prefix: c,
                depth: 16,
            },
        ];
        assert_matches_per_tile_search(seg, &tiles, &format!("cell {c} after cell {}", c - 1));
    }
}

#[test]
fn nested_tiles_among_empty_ones_are_each_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];
    // A sparse depth-12 request with each tile's sixteen depth-14 children beside it: the first
    // child shares its parent's low bound, and the children end before the parent does, so the
    // high bounds do not ascend.
    let parents = tiles_for_bbox([0.10, 0.20, 0.16, 0.26], 12, &unit_extent());
    let mut tiles = parents.clone();
    for parent in &parents {
        tiles.extend((0..16).map(|child| Tile {
            prefix: parent.prefix << 4 | child,
            depth: 14,
        }));
    }
    assert_matches_per_tile_search(seg, &tiles, "parents then children");
    tiles.reverse();
    assert_matches_per_tile_search(seg, &tiles, "children then parents");
}

#[test]
fn a_tile_is_found_when_a_tile_nested_in_it_ends_before_its_first_cell() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, mut codes) = build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];
    codes.sort_unstable();
    codes.dedup();
    let codes: Vec<u64> = codes.into_iter().map(u64::from).collect();
    let mut checked = 0;
    // For each depth-12 tile whose first cell `c` is at least two cells in, with the cell below
    // the tile empty: that empty cell, the tile, and the empty cell one past the tile's low bound,
    // which lies inside the tile and ends before `c`. In code order the high bounds do not ascend.
    for (at, &c) in codes.iter().enumerate() {
        let low = c >> 8 << 8;
        let first_in_tile = at == 0 || codes[at - 1] < low;
        if !first_in_tile || low == 0 || c < low + 2 || codes.binary_search(&(low - 1)).is_ok() {
            continue;
        }
        let tiles = [
            Tile {
                prefix: low - 1,
                depth: 16,
            },
            Tile {
                prefix: c >> 8,
                depth: 12,
            },
            Tile {
                prefix: low + 1,
                depth: 16,
            },
        ];
        assert_matches_per_tile_search(seg, &tiles, &format!("the depth-12 tile holding {c}"));
        checked += 1;
    }
    assert!(checked > 0, "the fixture has such a tile");
}

/// A random depth and an in-range prefix for it.
fn random_tile(rng: &mut impl rand::Rng) -> Tile {
    let depth: u8 = rng.gen_range(0..=16);
    let prefix = if depth == 0 {
        0
    } else {
        rng.gen::<u64>() & ((1u64 << (2 * depth as u32)) - 1)
    };
    Tile { prefix, depth }
}

#[test]
fn tile_ranges_all_agrees_with_the_full_column_search_over_depths_bboxes_and_orders() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 5,000 items over `build_bundle`'s 100x100 position grid, so every Morton code is repeated
    // ~50 times — which is exactly where a sweep that floors the next search at the previous
    // tile's *end* rather than its *start* begins to lie.
    let (_items, _codes) = build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];
    let extent = unit_extent();

    // Every bbox shape a viewport request can produce a tile set from, including the ones a
    // hand-written sweep tends to skip: corners given in reverse, a degenerate single-point box,
    // the full extent, and boxes that clamp at an edge.
    let bboxes: [(&str, [f64; 4]); 7] = [
        ("interior", [0.10, 0.20, 0.40, 0.55]),
        ("reversed corners", [0.40, 0.55, 0.10, 0.20]),
        ("degenerate point", [0.33, 0.33, 0.33, 0.33]),
        ("full extent", [0.0, 0.0, 1.0, 1.0]),
        ("clamped below", [-5.0, -5.0, 0.05, 0.05]),
        ("clamped above", [0.95, 0.95, 5.0, 5.0]),
        ("straddling the quadrant split", [0.45, 0.45, 0.55, 0.55]),
    ];

    for (label, bbox) in bboxes {
        // Depth is capped at 6 because the full-extent bbox enumerates the *whole* grid at that
        // depth (4^6 = 4,096 tiles, x3 orderings, x7 bboxes); the property under test is
        // depth-independent, and `tile_ranges_all_agrees_..._for_arbitrary_tile_sets` below
        // reaches depth 16 directly.
        for depth in 0u8..=6 {
            let tiles = tiles_for_bbox(bbox, depth, &extent);
            assert_matches_per_tile_search(seg, &tiles, &format!("{label} d{depth} as-enumerated"));

            // Reversed: an implementation that leaked its own sweep order into the result can
            // still coincide with raster order on some inputs, but not on this one.
            let mut reversed = tiles.clone();
            reversed.reverse();
            assert_matches_per_tile_search(seg, &reversed, &format!("{label} d{depth} reversed"));

            // Every tile twice, adjacent: a repeated code range must resolve identically both
            // times.
            let doubled: Vec<Tile> = tiles.iter().flat_map(|t| [*t, *t]).collect();
            assert_matches_per_tile_search(seg, &doubled, &format!("{label} d{depth} doubled"));
        }
    }
}

#[test]
fn tile_ranges_all_agrees_with_the_full_column_search_for_arbitrary_tile_sets() {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, _codes) = build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];

    // Randomised rather than `proptest`-generated so the bundle fixture (tempdir, Arrow write,
    // real SHA-256 digests) is built once instead of once per generated case. The seed is fixed,
    // so any failure reproduces by running this test again.
    let mut rng = StdRng::seed_from_u64(0x7E55E4A);
    for case in 0..2_000u32 {
        let n = rng.gen_range(0..24usize);
        // Mixed depths in one set — the case where ordering the sweep by `prefix` instead of by
        // the code-range low bound gives a wrong visit order and a truncated range.
        let tiles: Vec<Tile> = (0..n).map(|_| random_tile(&mut rng)).collect();
        assert_matches_per_tile_search(seg, &tiles, &format!("random case {case}"));
    }
}

#[test]
fn tile_ranges_all_over_an_empty_tile_set_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 64);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.only_shard().views["main"].segments[0];
    assert!(mosaica_store::tile_ranges_all(seg, &[]).is_empty());
}

/// **The fourth constructor: a prefix this process just wrote opens without re-hashing it.**
///
/// [`open_written_prefix`] exists because a fold must serve from a prefix it has *this moment*
/// finished writing and digesting, in-process (compaction §4 step 5, decision D1). It is the only
/// route that opens a prefix by name rather than through `CURRENT`, and the only one that skips the
/// digest sweep and `Permutation::validate_rows`.
///
/// Asserted two ways, because the constructor's value is exactly the difference between them.
#[test]
fn a_just_written_prefix_opens_to_the_same_bundle_without_re_verifying_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (items, codes) = build_bundle(dir.path(), 200);

    let verified = open_bundle(dir.path()).expect("the read protocol in full");
    let trusted = mosaica_store::open_written_prefix(dir.path(), "v00000")
        .expect("a prefix this process wrote");

    // Same bundle, by every structure a request reads.
    let a = &verified.only_shard().views["main"].segments[0];
    let b = &trusted.only_shard().views["main"].segments[0];
    assert_eq!(a.seg_id, b.seg_id);
    assert_eq!(a.row_count, items.len() as u32);
    assert_eq!(b.row_count, a.row_count);
    assert_eq!(b.morton.u32(), codes.as_slice());
    assert_eq!(b.columns.mosaica_id(), a.columns.mosaica_id());
    assert_eq!(
        verified.partitions["default"].segments_n,
        trusted.partitions["default"].segments_n
    );

    // And it does not read `CURRENT` at all — the fold calls it after the flip, but naming the
    // prefix is what makes the call independent of the commit having landed.
    fs::remove_file(dir.path().join("CURRENT")).expect("remove CURRENT");
    mosaica_store::open_written_prefix(dir.path(), "v00000")
        .expect("the prefix is named, not looked up");
}

/// **What the trusted constructor skips is bytes-from-storage checks, and nothing else.**
///
/// A file whose content no longer matches its digest opens here — that is the whole premise, and
/// stating it as a test is what stops the premise from being quietly widened. `open_bundle` on the
/// same tree fails closed, which is the half that must never change.
#[test]
fn a_just_written_prefix_skips_the_digest_sweep_and_open_bundle_does_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 40);

    let columns_path = dir
        .path()
        .join("v00000")
        .join(seg_file("seg0", "columns.arrow"));
    let mut bytes = fs::read(&columns_path).expect("read columns.arrow");
    // Mid-file, inside the record batch body rather than the footer, so the file still *parses*:
    // the only thing that separates it from the original is the digest, which is exactly the check
    // under test.
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&columns_path, &bytes).expect("rewrite columns.arrow");

    assert!(
        open_bundle(dir.path()).is_err(),
        "a bundle whose bytes were not checked is a bundle whose authorisation data was not \
         checked — this direction is unconditional and must stay so"
    );
    assert!(
        mosaica_store::open_written_prefix(dir.path(), "v00000").is_ok(),
        "and the trusted constructor does not re-derive a digest its caller just computed"
    );
}

/// **The structural checks are kept**, so a self-inconsistent manifest fails closed on this route
/// too. `row_count` disagreeing with the file it names is a writer bug, not a storage fault, and
/// the premise ("this process wrote these bytes") does not cover it.
#[test]
fn a_just_written_prefix_still_refuses_a_manifest_that_disagrees_with_its_own_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 40);

    edit_segments_manifest(dir.path(), |value| {
        shard0(value)["segments"][0]["row_count"] = serde_json::json!(39);
    });

    let err = mosaica_store::open_written_prefix(dir.path(), "v00000")
        .expect_err("the manifest disagrees with morton.u32 and columns.arrow");
    assert!(
        matches!(err, StoreError::MalformedBundle { .. }),
        "expected MalformedBundle, got: {err}"
    );
}

// -------------------------------------------------------------------------------------------
// Non-canonical `SEGMENTS-<n>.json` names (contracts §2.1).
//
// `n` is unpadded decimal. A reader that parses `SEGMENTS-01.json` to `n = 1` and then
// reconstructs `SEGMENTS-1.json` to read from discovers a manifest and then reads from an absent
// path — which the candidate walk steps silently past, carrying the reader past a manifest that
// may hold a `deny`. §2.1: "Parsing leniently and reconstructing canonically is the combination
// that hides it."
// -------------------------------------------------------------------------------------------

/// Write publication 0 again with `edit` applied, its partition file under a literal `name` and
/// its shard files at `version`. Unlike [`add_segments_manifest`] the name is not derived from the
/// number, which is the whole point: these fixtures write names the writer cannot produce, because
/// the file has to arrive from outside it — an operator copy, a partial object-store sync, another
/// implementation.
fn add_segments_manifest_named(
    root: &Path,
    name: &str,
    version: u64,
    edit: impl FnOnce(&mut serde_json::Value),
) {
    let prefix_dir = root.join("v00000");
    let mut value = read_assembled(&prefix_dir, 0);
    edit(&mut value);
    write_assembled_named(&prefix_dir, name, version, value);
}

/// **The disclosure this refusal exists to close.** A zero-padded `SEGMENTS-01.json` carrying an
/// accepted suppression is present in the prefix. A reader that parses it to `n = 1` reads
/// `SEGMENTS-1.json`, which does not exist, records an I/O error and steps down to a
/// `SEGMENTS-0.json` that verifies perfectly — in which entity 17 is visible again. Not an error
/// of the wrong type: an `Ok` that serves the pre-suppression state, indistinguishable from a
/// prefix that never carried the deny at all.
///
/// The refusal converts that step-past into a loud, typed error naming the file.
///
/// Mutations this kills: dropping the canonical-spelling check in
/// `list_segments_manifests` (a bare `rest.parse::<u64>()`, or a `continue`/skip in place of the
/// refusal) — the padded deny is then stepped past and `open_bundle` returns the suppressed
/// entity; and typing the refusal as `NoVerifyingSegmentsManifest`, which is the "absent
/// manifest" report this must never be confused with.
#[test]
fn a_padded_manifest_carrying_a_deny_is_refused_never_stepped_past() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    add_segments_manifest_named(dir.path(), "SEGMENTS-01.json", 1, |value| {
        shard0(value)["deny"] = serde_json::to_value(deny_set(&[17])).unwrap();
    });

    match open_bundle(dir.path()).expect_err(
        "a padded candidate must be refused by name; parsing it and reading the unpadded name \
         steps past a manifest carrying an accepted suppression",
    ) {
        StoreError::NonCanonicalManifestName { name, .. } => {
            assert_eq!(name, "SEGMENTS-01.json");
        }
        other => panic!(
            "expected NonCanonicalManifestName — anything else leaves the padded deny \
             indistinguishable from an absent manifest. Got: {other}"
        ),
    }
}

/// The refusal is by *name*, before any read: a padded candidate is refused whether or not it
/// carries a deny, because the reader cannot know which it is without opening it, and opening it
/// is what the canonical reconstruction prevents.
#[test]
fn a_padded_manifest_is_refused_even_carrying_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    add_segments_manifest_named(dir.path(), "SEGMENTS-007.json", 7, |_| {});

    match open_bundle(dir.path()).expect_err("a padded candidate is refused by name") {
        StoreError::NonCanonicalManifestName { name, .. } => {
            assert_eq!(name, "SEGMENTS-007.json")
        }
        other => panic!("expected NonCanonicalManifestName, got: {other}"),
    }
}

/// A candidate whose numeric part is not a decimal number at all is non-canonical for the same
/// reason and refused the same way — an empty part, a sign, whitespace, or a value past `u64`.
/// None of these is a spelling of a number this reader could re-render, so none can be admitted
/// without inventing a name the format does not define.
#[test]
fn other_non_canonical_numeric_parts_are_refused_too() {
    for name in [
        "SEGMENTS-.json",
        "SEGMENTS-+1.json",
        "SEGMENTS- 1.json",
        "SEGMENTS-1 .json",
        "SEGMENTS-latest.json",
        "SEGMENTS-99999999999999999999.json",
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        build_bundle(dir.path(), 50);
        add_segments_manifest_named(dir.path(), name, 1, |_| {});

        match open_bundle(dir.path())
            .expect_err(&format!("'{name}' is not a canonical SEGMENTS-<n>.json"))
        {
            StoreError::NonCanonicalManifestName { name: got, .. } => assert_eq!(got, name),
            other => panic!("expected NonCanonicalManifestName for '{name}', got: {other}"),
        }
    }
}

/// **The control.** `SEGMENTS-0.json` is canonical — a rule that refuses a legitimate zero is
/// worse than the gap it closes — and so is a multi-digit `n`. Both open, and the walk still
/// serves the highest.
#[test]
fn canonical_zero_and_multi_digit_names_still_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);
    add_segments_manifest(dir.path(), 11, |_| {});

    let bundle = open_bundle(dir.path()).expect("canonical names open");
    assert_eq!(
        bundle.partitions["default"].segments_n, 11,
        "SEGMENTS-11.json is canonical and is the highest candidate"
    );
}

/// A file that is not a `SEGMENTS-<n>.json` candidate at all is not swept up by the refusal. The
/// `.json.tmp` orphan a crashed manifest write leaves behind (`manifest_write`) is the case that
/// matters: it is expected residue, not a manifest, and turning it into a hard partition failure
/// would refuse a bundle that is entirely well-formed.
#[test]
fn a_crashed_write_orphan_is_not_a_candidate() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);
    let partition = partition_dir(dir.path());
    fs::write(partition.join("SEGMENTS-1.json.tmp"), b"{}").expect("write orphan");
    fs::write(partition.join("notes.txt"), b"hello").expect("write stray");

    let bundle = open_bundle(dir.path()).expect("a .json.tmp orphan is not a manifest candidate");
    assert_eq!(bundle.partitions["default"].segments_n, 0);
}

// -------------------------------------------------------------------------------------------
// The commit point over split files: a partition `SEGMENTS-<n>.json` names each shard's own file
// by number and digest, and a candidate steps down or is refused whole.
// -------------------------------------------------------------------------------------------

/// The fixture's bundle with an empty shard 1 beside shard 0, published at 1, then at 2 with
/// `edit` applied to shard 1's record. Returns the prefix directory.
fn two_shards_published(
    root: &Path,
    edit: impl FnOnce(&mut mosaica_store::manifest::ShardRecord),
) -> std::path::PathBuf {
    build_bundle(root, 50);
    let prefix_dir = root.join("v00000");
    let published = mosaica_store::read_publication(&prefix_dir, "default", 0).unwrap();
    let mut manifest = published.manifest;
    add_shard_one(&mut manifest);
    let first = mosaica_store::write_publication(&prefix_dir, "default", 1, &manifest, None)
        .expect("publication 1");
    edit(&mut manifest.shards.records[1]);
    let second =
        mosaica_store::write_publication(&prefix_dir, "default", 2, &manifest, Some(&first))
            .expect("publication 2");
    assert_eq!(
        (second.shards[0].n, second.shards[1].n),
        (1, 2),
        "only shard 1's file was written again"
    );
    prefix_dir
}

/// **A shard file that fails its digest steps the whole partition down**: both shards are served
/// from the publication below, never shard 0 from one and shard 1 from another.
#[test]
fn a_corrupt_shard_file_steps_the_whole_partition_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prefix_dir = two_shards_published(dir.path(), |record| record.high_water = 9);
    let path = shard_file(&prefix_dir, 1, 2);
    let mut bytes = fs::read(&path).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x20;
    fs::write(&path, bytes).unwrap();

    let bundle = open_bundle(dir.path()).expect("the partition steps down");
    let partition = &bundle.partitions["default"];
    assert_eq!(partition.segments_n, 1);
    assert!(partition.stepped_down());
    let high_waters: Vec<u64> = partition
        .shards
        .iter()
        .map(|shard| shard.record.high_water)
        .collect();
    assert_eq!(
        high_waters,
        vec![50, 0],
        "both shards as publication 1 left them"
    );
}

/// **A candidate whose partition file flags deny state is refused when a file it names is
/// missing**, before that file could say what it denied.
#[test]
fn a_deny_carrying_candidate_with_a_missing_shard_file_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prefix_dir = two_shards_published(dir.path(), |record| record.deny = deny_set(&[3]));
    fs::remove_file(shard_file(&prefix_dir, 1, 2)).unwrap();

    match open_bundle(dir.path()).expect_err("a deny-carrying candidate is never stepped past") {
        StoreError::UnverifiedDenyManifest { n, .. } => assert_eq!(n, 2),
        other => panic!("expected UnverifiedDenyManifest, got: {other}"),
    }
}

/// **Where nothing verifies, the refusal names the shard file that failed.**
#[test]
fn a_refusal_names_the_shard_file_that_failed() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);
    let path = shard_file(&dir.path().join("v00000"), 0, 0);
    let mut bytes = fs::read(&path).unwrap();
    bytes.push(b' ');
    fs::write(&path, bytes).unwrap();

    match open_bundle(dir.path()).expect_err("nothing verifies") {
        StoreError::NoVerifyingSegmentsManifest {
            highest_candidate_error: Some(reason),
            ..
        } => assert!(
            reason.contains("shards/0/SEGMENTS-0.json"),
            "the reason names the shard file: {reason}"
        ),
        other => panic!("expected NoVerifyingSegmentsManifest, got: {other}"),
    }
}
