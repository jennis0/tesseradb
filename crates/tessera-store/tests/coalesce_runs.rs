//! The external-id run merge: key order, and **the keep-newest rule** (decision 0047).
//!
//! `merge_execution::the_external_id_runs_coalesce_in_key_order` covers the ordering over runs
//! with disjoint keys, which is the easy half. What it cannot see is the tie-break, because its
//! fixture never puts one key in two runs — and the tie-break is the half where being wrong is
//! silent and serves a *deleted* entity under a live caller's key.
//!
//! **Why a key is ever in two runs.** Decision 0047 makes an edit a delete plus a re-ingest, and a
//! re-ingest re-binds the external id to a **new** entity. The old holder is a forgotten, deleted
//! entity whose binding is still on disc in an older run. The reader resolves newest-run-first, so
//! a coalesced run must answer exactly as the runs it replaced did: the newest binding wins. Get
//! the tie-break backwards and `/v1/items` answers a deleted entity for a live key — with the
//! right shape, the right count and no error anywhere.

mod fixture;

use std::path::{Path, PathBuf};

use arrow::array::{Array, BinaryArray, UInt32Array};
use fixture::{build_bundle, PARTITION, VIEW};
use tessera_store::coalesce_external_id_runs;
use tessera_store::flush::{write_flush_segment, FlushInput, FlushRow};
use tessera_store::manifest::Quantisation;
use tessera_types::{EntityId, IdentityKey, ROW_ABSENT};

fn key() -> IdentityKey {
    IdentityKey::from_hex("0123456789abcdef0123456789abcdef").expect("test key")
}

fn quantisation() -> Quantisation {
    Quantisation {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

/// Write a segment whose rows carry the given `(entity, external_id)` bindings, and return the
/// path of its `external-ids.arrow` run.
fn run_of(root: &Path, seg_id: &str, bindings: &[(u64, &str)]) -> PathBuf {
    let rows: Vec<FlushRow> = bindings
        .iter()
        .map(|(entity, external_id)| FlushRow {
            entity_id: EntityId::new(*entity),
            external_id: Some(external_id.as_bytes().to_vec()),
            x: ((*entity % 97) as f64) / 97.0,
            y: ((*entity % 89) as f64) / 89.0,
            scalars: vec![],
        })
        .collect();
    write_flush_segment(
        &root.join("v00000"),
        PARTITION,
        VIEW,
        FlushInput {
            incarnation: 0,
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
        .join("views")
        .join(VIEW)
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

/// **A key present in two runs keeps the newest binding, and appears exactly once.**
///
/// `runs` arrives oldest-first, so the *later* path in the slice is the newer binding. The
/// streaming k-way merge pops equal keys in ascending run order and emits only the last of each
/// group; a heap that tie-broke on descending run index, or a keep-*first* pass, would emit the
/// deleted holder instead — same row count, same ordering, wrong entity.
///
/// **Mutation:** flip the keep-last lookahead in `coalesce::merge_runs` to keep-first, or reverse
/// the run tie-break, and `shared` resolves to 101 rather than 201.
#[test]
fn a_key_in_two_runs_keeps_the_newest_binding() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);

    let old = run_of(
        dir.path(),
        "run-old",
        &[(100, "alpha"), (101, "shared"), (102, "beta")],
    );
    let new = run_of(
        dir.path(),
        "run-new",
        &[(200, "delta"), (201, "shared"), (202, "gamma")],
    );

    let out_dir = dir.path().join("coalesced");
    std::fs::create_dir_all(&out_dir).unwrap();
    let rows = coalesce_external_id_runs(&[old, new], 100, 202, &out_dir).expect("coalesce runs");

    let pairs = pairs_of(&out_dir.join("external-ids.arrow"));
    assert_eq!(rows, 5, "six bindings over five distinct keys");
    assert_eq!(pairs.len(), 5);

    let shared: Vec<u32> = pairs
        .iter()
        .filter(|(k, _)| k == b"shared")
        .map(|(_, e)| *e)
        .collect();
    assert_eq!(
        shared,
        vec![201],
        "the newest binding wins and the older one is gone — a re-ingest re-binds the key \
         (decision 0047), so 101 is a forgotten, deleted holder"
    );

    let keys: Vec<&[u8]> = pairs.iter().map(|(k, _)| k.as_slice()).collect();
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "strictly ascending, which is what the sidecar binary-searches: {keys:?}"
    );
}

/// **The locator agrees with the run about which ordinal a surviving key sits at**, including for
/// the key whose older binding was dropped.
///
/// The two directions are written by the same pass and a reader trusts them jointly: the forward
/// run answers `external_id → entity`, the locator answers `entity → ordinal in this run`. A
/// keep-newest rule applied to one and not the other leaves the reverse direction pointing at the
/// row the merge discarded.
#[test]
fn the_locator_points_at_the_surviving_ordinal() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);

    let old = run_of(dir.path(), "loc-old", &[(100, "alpha"), (101, "shared")]);
    let new = run_of(dir.path(), "loc-new", &[(201, "shared"), (202, "zeta")]);

    let out_dir = dir.path().join("coalesced");
    std::fs::create_dir_all(&out_dir).unwrap();
    coalesce_external_id_runs(&[old, new], 100, 202, &out_dir).expect("coalesce runs");

    let pairs = pairs_of(&out_dir.join("external-ids.arrow"));
    let locator = locator_of(&out_dir);
    assert_eq!(locator.len(), 103, "the span is the caller's: 100..=202");

    for (ordinal, (key, entity)) in pairs.iter().enumerate() {
        let slot = (*entity as usize) - 100;
        assert_eq!(
            locator[slot], ordinal as u32,
            "entity {entity} holds {key:?} at ordinal {ordinal}, and the locator must say so"
        );
    }

    // 101's binding was superseded, so it has no ordinal at all — not ordinal 0, which is a real
    // row belonging to a real key.
    assert_eq!(
        locator[101 - 100],
        ROW_ABSENT,
        "the dropped holder must be absent from the reverse direction, not aliased onto row 0"
    );
    // And an entity that never carried an external id is absent too (contracts §3.4's ordinary
    // case), rather than the span ending short of it.
    assert_eq!(locator[150 - 100], ROW_ABSENT);
}

/// An entity holding a key but sitting outside the caller's span is a **typed refusal**, not a
/// silently dropped binding: the reverse direction would have no home for it, and a drill-down
/// would then get "no external id" for an item that has one.
#[test]
fn a_binding_outside_the_span_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    build_bundle(dir.path(), 10);

    let run = run_of(dir.path(), "oob", &[(100, "alpha"), (500, "beta")]);
    let out_dir = dir.path().join("coalesced");
    std::fs::create_dir_all(&out_dir).unwrap();

    let err = coalesce_external_id_runs(&[run], 100, 202, &out_dir)
        .expect_err("a binding past the span must not be written");
    assert!(
        err.to_string().contains("500"),
        "the refusal must name the entity: {err}"
    );
}
