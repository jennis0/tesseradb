//! Compaction's pass 3 (compaction §3, "Pass 3 — external ids"): the fold's own entry point,
//! [`fold_external_id_runs`] — the same keep-newest merge `coalesce_runs.rs` pins, plus the two
//! things only the fold needs: dropping a tombstoned entity's key, and a locator written through a
//! mapping rather than an in-memory `Vec`.
//!
//! **The fatal trap this file exists to catch** (r3, and "fatal as first written"): a locator
//! sized to the *live* entity space rather than the snapshot's swallows every post-snapshot entity
//! and answers "no external id" for one that has one
//! (`a_post_snapshot_entity_resolves_through_its_carried_forward_extent`, contracts §2.4).

use std::fs;
use std::path::{Path, PathBuf};

use arrow::array::{Array, BinaryArray, UInt32Array};
use croaring::Bitmap;
use sha2::{Digest, Sha256};

use tessera_store::flush::{write_flush_segment, FlushInput, FlushRow};
use tessera_store::manifest::{FileDigest, Quantisation};
use tessera_store::{fold_external_id_runs, open_bundle, ExternalIdSidecar};
use tessera_types::{EntityId, IdentityKey, ROW_ABSENT};

mod fixture;
use fixture::{build_bundle, PARTITION, SLICE};

const KEY_HEX: &str = "0123456789abcdef0123456789abcdef";

fn key() -> IdentityKey {
    IdentityKey::from_hex(KEY_HEX).expect("test key")
}

fn quantisation() -> Quantisation {
    Quantisation {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

/// `tombstones`, croaring-shaped — the same helper `fold_row_space.rs` uses for pass 1, so the two
/// passes' fixtures read the same way.
fn tombstones(entities: &[u64]) -> Bitmap {
    Bitmap::of(&entities.iter().map(|&e| e as u32).collect::<Vec<u32>>())
}

/// Write a segment whose rows carry the given `(entity, external_id)` bindings, and return the
/// path of its `external-ids.arrow` run — an input run for the fold to merge, not itself part of
/// the fold's own output. Matches `coalesce_runs.rs`'s `run_of`, which this pass's tests are the
/// direct sibling of.
fn run_of(root: &Path, seg_id: &str, bindings: &[(u64, &str)]) -> PathBuf {
    let rows: Vec<FlushRow> = bindings
        .iter()
        .map(|(entity, external_id)| FlushRow {
            entity_id: EntityId::new(*entity),
            external_id: Some(external_id.as_bytes().to_vec()),
            x: ((*entity % 97) as f32) / 97.0,
            y: ((*entity % 89) as f32) / 89.0,
            scalars: vec![],
        })
        .collect();
    write_flush_segment(
        &root.join("v00000"),
        PARTITION,
        SLICE,
        FlushInput {
            seg_id,
            rows,
            quantisation: quantisation(),
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &[],
            row_base: 0,
        },
    )
    .expect("the input segment writes");
    root.join("v00000/partitions")
        .join(PARTITION)
        .join("slices")
        .join(SLICE)
        .join("segments")
        .join(seg_id)
        .join("external-ids.arrow")
}

/// Every `(external_id, entity)` pair in a written run, in file order.
fn pairs_of(path: &Path) -> Vec<(Vec<u8>, u32)> {
    let file = std::fs::File::open(path).expect("the run opens");
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).expect("arrow reads it");
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.expect("a batch decodes");
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .expect("column 0 is binary");
        let entities = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("column 1 is uint32");
        for i in 0..batch.num_rows() {
            out.push((ids.value(i).to_vec(), entities.value(i)));
        }
    }
    out
}

fn locator_of(dir: &Path) -> Vec<u32> {
    std::fs::read(dir.join("ext-locator.u32"))
        .expect("the locator is written")
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn flush_row(entity: u64, external_id: Option<&[u8]>, x: f32, y: f32) -> FlushRow {
    FlushRow {
        entity_id: EntityId::new(entity),
        external_id: external_id.map(|id| id.to_vec()),
        x,
        y,
        scalars: vec![],
    }
}

fn file_digest(path: &Path) -> FileDigest {
    let bytes = fs::read(path).expect("read for digest");
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    FileDigest {
        size: bytes.len() as u64,
        sha256: hex,
    }
}

/// **Every external id resolves both ways after the fold.** Run 0 answers `key -> entity`, the
/// locator answers `entity -> ordinal in run 0`, and the two agree — the fold-entry-point
/// counterpart of `coalesce_runs::the_locator_points_at_the_surviving_ordinal`. No tombstones
/// here: this test is about the merge landing correctly through [`fold_external_id_runs`], not
/// about the filter (see the tombstone-specific tests below for that).
///
/// **Mutation:** swap `locator.set` and `writer.append`'s order in `merge_runs_core`'s `emit`, or
/// feed the wrong `rows` counter as the ordinal, and this fails — the locator would point at the
/// wrong row.
#[test]
fn every_live_external_id_resolves_both_ways_after_the_fold() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);

    let a = run_of(dir.path(), "run-a", &[(100, "alpha"), (102, "beta")]);
    let b = run_of(dir.path(), "run-b", &[(200, "gamma"), (202, "delta")]);

    let out_dir = dir.path().join("entities");
    fs::create_dir_all(&out_dir).unwrap();
    let rows = fold_external_id_runs(&[a, b], 100, 202, &Bitmap::new(), &out_dir)
        .expect("fold merges the live runs");
    assert_eq!(rows, 4);

    let pairs = pairs_of(&out_dir.join("external-ids.arrow"));
    assert_eq!(
        pairs,
        vec![
            (b"alpha".to_vec(), 100),
            (b"beta".to_vec(), 102),
            (b"delta".to_vec(), 202),
            (b"gamma".to_vec(), 200),
        ],
        "run 0 holds every live binding, sorted by key"
    );

    let locator = locator_of(&out_dir);
    assert_eq!(locator.len(), 103, "the span is the caller's: 100..=202");
    for (ordinal, (k, entity)) in pairs.iter().enumerate() {
        let slot = (*entity as usize) - 100;
        assert_eq!(
            locator[slot], ordinal as u32,
            "entity {entity} holds {k:?} at ordinal {ordinal}, and the locator must say so"
        );
    }
    // An entity that never carried an external id sits at the sentinel, not aliased onto row 0.
    assert_eq!(locator[101 - 100], ROW_ABSENT);
}

/// **A folded-away entity's key is gone from run 0**, and its locator slot is the absent
/// sentinel rather than a dangling ordinal — compaction §3's pass 3 obligation. Decision 0047
/// needs this because `tessera-engine`'s `established_collisions` exempts a holder from the
/// ingest duplicate check only while `overlay.is_deleted` is true, and retirement
/// (`Overlay::retire`, elsewhere) makes that false; a binding left standing here would then
/// refuse a lawful re-ingest of the key with a 409.
///
/// **Left to an engine-level test, deliberately**: the 200-on-re-ingest half, since clearing the
/// live `established` map this drop must also be paired with lives in `tessera-engine`, not in
/// this crate (see `coalesce.rs`'s module doc).
///
/// **Mutation:** drop the `is_tombstoned` check from `merge_runs_core`'s emit path, and `"gone"`
/// still appears in `pairs` bound to entity 101.
#[test]
fn a_tombstoned_entitys_key_is_gone_from_run_zero() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);

    let run = run_of(
        dir.path(),
        "run-live",
        &[(100, "alpha"), (101, "gone"), (102, "beta")],
    );

    let out_dir = dir.path().join("entities");
    fs::create_dir_all(&out_dir).unwrap();
    let rows = fold_external_id_runs(&[run], 100, 102, &tombstones(&[101]), &out_dir)
        .expect("fold drops the tombstoned key");
    assert_eq!(rows, 2, "two of the three bindings survive");

    let pairs = pairs_of(&out_dir.join("external-ids.arrow"));
    assert!(
        pairs.iter().all(|(k, _)| k != b"gone"),
        "the tombstoned entity's key must not survive the fold: {pairs:?}"
    );

    let locator = locator_of(&out_dir);
    assert_eq!(
        locator[101 - 100],
        ROW_ABSENT,
        "no ordinal for a dropped entity, not an ordinal into a row that isn't there"
    );
}

/// **A key whose newest binding is tombstoned drops entirely — it must not fall back to an
/// older, non-tombstoned run.** Decision 0047 makes an older binding of a re-bound key a
/// forgotten, deleted holder regardless of whether the newer one is *also* tombstoned; there is
/// nothing live to fall back to. An implementation that filtered tombstoned cursors out of the
/// heap before the keep-newest pass runs, rather than after it settles on a survivor, would get
/// this backwards and resurrect the older holder.
///
/// **Mutation:** filter tombstoned entities out of the merge input before the keep-newest pass
/// instead of after it picks the survivor, and `"shared"` resurfaces bound to entity 101.
#[test]
fn a_tombstoned_newest_binding_drops_the_key_rather_than_falling_back() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);

    let old = run_of(dir.path(), "run-old", &[(101, "shared")]);
    let new = run_of(dir.path(), "run-new", &[(201, "shared")]);

    let out_dir = dir.path().join("entities");
    fs::create_dir_all(&out_dir).unwrap();
    // 201 (the newest binding) is tombstoned; 101 (the older, already-superseded one) is not.
    let rows = fold_external_id_runs(&[old, new], 101, 201, &tombstones(&[201]), &out_dir)
        .expect("fold runs");
    assert_eq!(rows, 0, "the only key present has no live binding left");

    let pairs = pairs_of(&out_dir.join("external-ids.arrow"));
    assert!(
        pairs.is_empty(),
        "must not resurrect the older, already-forgotten holder: {pairs:?}"
    );
    let locator = locator_of(&out_dir);
    assert_eq!(locator[101 - 101], ROW_ABSENT);
    assert_eq!(locator[201 - 101], ROW_ABSENT);
}

/// **The fatal trap** (r3 — fatal as first written). `ExternalIdSidecar::external_id_of_checked`
/// gives the base locator absolute priority for any `entity < locator_len()` and consults the
/// carried-forward `locator_extents` only *past* it (contracts §2.4). This test builds the fold's
/// actual pass-3 output — a snapshot-bounded run 0 and locator over `[0, n)` — beside a real
/// post-snapshot flush's own run and locator extent, then resolves through the real sidecar
/// exactly as `Engine::external_id_of` would. A locator sized to the live entity space instead of
/// the snapshot's would swallow the post-snapshot entity's slot (never written by pass 3, so
/// still the absent sentinel) and answer `None` for an item that has a key.
///
/// **Mutation:** pass the live high-water (`n + 3`) instead of the snapshot's (`n - 1`) as
/// `entity_hi` to `fold_external_id_runs`, and `external_id_of_checked(EntityId::new(10), ..)`
/// answers `Ok(None)` instead of `Some(b"omega")` — a wrong answer wearing a legitimate state's
/// clothes (contracts §2.4).
#[test]
fn a_post_snapshot_entity_resolves_through_its_carried_forward_extent() {
    let dir = tempfile::TempDir::new().unwrap();
    let n = 10u64; // the snapshot's entity space: [0, n)
    build_bundle(dir.path(), n);
    let prefix = dir.path().join("v00000");

    // Pass 3: one live run over the snapshot's own entities, no tombstones — the fold's own
    // output, bounded at [0, n).
    let run0_src = run_of(dir.path(), "pre-fold", &[(3, "zeta"), (7, "alpha")]);
    let entities_dir = prefix.join("entities");
    fs::create_dir_all(&entities_dir).unwrap();
    fold_external_id_runs(&[run0_src], 0, n - 1, &Bitmap::new(), &entities_dir)
        .expect("pass 3 folds the snapshot's runs");

    // A post-snapshot flush: entities [n, n + 3), published exactly as a real flush would,
    // including its own run and locator extent.
    let out = write_flush_segment(
        &prefix,
        PARTITION,
        SLICE,
        FlushInput {
            seg_id: "post-fold-flush",
            rows: vec![
                flush_row(n, Some(b"omega"), 0.1, 0.1),
                flush_row(n + 1, None, 0.2, 0.2),
                flush_row(n + 2, Some(b"iota"), 0.3, 0.3),
            ],
            quantisation: quantisation(),
            identity_key: &key(),
            shard_id: 0,
            scalar_schema: &[],
            row_base: n as u32,
        },
    )
    .expect("the post-snapshot flush writes");

    let bundle = open_bundle(dir.path()).expect("bundle opens");
    let mut manifest = bundle.partitions[PARTITION].manifest.clone();
    // Run 0 (the fold's) listed first — oldest — with the flush's run after it, matching what a
    // real fold's publication carries forward (compaction §2, §4).
    manifest.external_id_runs = vec![
        "entities/external-ids.arrow".to_string(),
        out.external_id_run.clone(),
    ];
    manifest.locator_extents = vec![out.locator_extent.clone()];
    manifest.files.insert(
        "entities/external-ids.arrow".to_string(),
        file_digest(&entities_dir.join("external-ids.arrow")),
    );
    manifest.files.insert(
        "entities/ext-locator.u32".to_string(),
        file_digest(&entities_dir.join("ext-locator.u32")),
    );
    manifest.files.extend(out.files.clone());

    let sidecar = ExternalIdSidecar::deferred_from_manifest(&bundle.manifest, &manifest, &prefix)
        .expect("the sidecar constructs over the fold's output plus the carried-forward extent");

    assert_eq!(
        sidecar.locator_len(),
        n,
        "the base locator is bounded at the snapshot, never widened to the live high-water"
    );

    // The post-snapshot entities: absent from the base locator's own span (never written by pass
    // 3), reachable only through the carried-forward extent.
    assert_eq!(
        sidecar
            .external_id_of_checked(EntityId::new(n), n + 3)
            .unwrap(),
        Some(b"omega".to_vec()),
        "a post-snapshot entity with an external id must still resolve"
    );
    assert_eq!(
        sidecar
            .external_id_of_checked(EntityId::new(n + 2), n + 3)
            .unwrap(),
        Some(b"iota".to_vec())
    );
    assert_eq!(
        sidecar
            .external_id_of_checked(EntityId::new(n + 1), n + 3)
            .unwrap(),
        None,
        "carried no external id at all — the ordinary case, not the inconsistency"
    );

    // A snapshot entity still resolves through pass 3's own output — the ordinary case this test
    // is not primarily about, but which the fixture must not accidentally have broken.
    assert_eq!(
        sidecar.external_id_of_checked(EntityId::new(7), n + 3).unwrap(),
        Some(b"alpha".to_vec())
    );

    // And the forward direction agrees, across both runs, newest-first (decision 0047).
    assert_eq!(sidecar.resolve(b"omega").unwrap(), Some(EntityId::new(n)));
    assert_eq!(sidecar.resolve(b"alpha").unwrap(), Some(EntityId::new(7)));
}

/// **The locator is written through a mapping, sized to the caller's span exactly** — the file's
/// byte length is `(entity_hi - entity_lo + 1) * 4` regardless of how few keys are actually live,
/// because the writer allocates the whole array up front rather than growing a buffer as pairs
/// arrive. `locator::LocatorWriter`'s own unit tests pin the pre-sizing property directly (that
/// type is crate-private and unreachable from an integration test); what this test can add from
/// outside the crate is that `fold_external_id_runs` really does route through it end to end.
///
/// **What this does NOT measure: peak RSS.** A span this small proves nothing about page-cache
/// behaviour under memory pressure — the property `LocatorWriter` exists for only bites at the
/// 4 GB-at-10⁹ scale compaction §3 describes, and a real measurement needs the probe harness
/// compaction §6.1's P3 already used
/// (`docs/evidence/memos/2026-08-05-compaction-flip-and-io.md`), not a unit test at this scale.
/// Stated plainly rather than implied: this is a file-size proxy, not an RSS measurement.
#[test]
fn the_locator_file_is_sized_to_the_callers_span_regardless_of_how_few_keys_are_live() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);

    let run = run_of(dir.path(), "sparse", &[(50_000, "only-one")]);
    let out_dir = dir.path().join("entities");
    fs::create_dir_all(&out_dir).unwrap();
    fold_external_id_runs(&[run], 0, 99_999, &Bitmap::new(), &out_dir).expect("fold runs");

    let len = fs::metadata(out_dir.join("ext-locator.u32")).unwrap().len();
    assert_eq!(
        len,
        100_000 * 4,
        "one live key over a 100,000-entity span still writes the whole array, not just the \
         key that landed in it"
    );
}
