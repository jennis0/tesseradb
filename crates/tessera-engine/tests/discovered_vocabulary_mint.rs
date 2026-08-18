//! Minting at the commit-window close (issue #82's engine half).
//!
//! `tessera-build`'s `discovered_vocabulary.rs` covers the build-time mint — a corpus supplying
//! keys the schema does not pin. This file covers the other producer: a novel key arriving through
//! `/control/ingest`, which travels as `WalScalar::Utf8` (`tessera-server`'s `category_code`
//! deliberately does not mint — see that function's own doc) and is resolved to a code once, on the
//! write executor, at the close of the commit window the row lands in.
//!
//! Every case here calls `Engine::accept_ingest` directly, the same boundary the HTTP handler calls
//! after it has already turned a discovered vocabulary's key into `WalScalar::Utf8` (or, for a
//! declared one, into its code). That is deliberate: it is the executor's own resolution this file
//! is pinning, not the handler's validation, which has no engine-crate seam to test from here.

mod common;

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::{Config, Schema};
use tessera_build::{build, BuildArgs};
use tessera_engine::{AcceptError, Engine, EngineConfig};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::{Wal, WalRecord, WalScalar};
use tessera_store::read::{open_bundle, ColumnsRef, ScalarSlice};
use tessera_types::EntityId;

/// A discovered `department` vocabulary, wide enough that ordinary tests never see exhaustion.
const DISCOVERED_WIDE: &str = r#"
[[vocabulary]]
name       = "department"
width      = "u16"
value_set  = "open"
visibility = "derived"

[[attribute]]
name       = "department"
type       = "category"
render     = true
vocabulary = "department"
"#;

/// A `u8` discovered `department` vocabulary with 250 of its 255 usable codes retired at build,
/// leaving exactly five free — 251..=255. Restart/never-reuse cases need the free space narrow
/// enough that a broken seed collides within a handful of draws rather than a cosmically unlikely
/// one; see `vocabulary.rs`'s own `a_draw_excludes_codes_from_every_home` for the same construction.
fn discovered_sparse_schema_toml() -> String {
    let retired: Vec<String> = (1..=250u32).map(|c| c.to_string()).collect();
    format!(
        r#"
[[vocabulary]]
name       = "department"
width      = "u8"
value_set  = "open"
visibility = "derived"
reserved   = [{}]

[[attribute]]
name       = "department"
type       = "category"
render     = true
vocabulary = "department"
"#,
        retired.join(", ")
    )
}

/// A **declared** `band` vocabulary — the closed-vocabulary sibling, for the case that pins the
/// executor's mint loop leaves an already-coded column untouched.
const DECLARED_BAND: &str = r#"
[[vocabulary]]
name       = "band"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  low = 1
  mid = 2
  high = 3

[[attribute]]
name       = "band"
type       = "category"
render     = true
vocabulary = "band"
"#;

fn parse_schema(tmp: &Path, text: &str) -> Schema {
    let path = tmp.join("config.toml");
    std::fs::write(&path, text).unwrap();
    Config::parse(&path, &std::collections::HashMap::new()).map(|c| c.schema).expect("the fixture schema parses")
}

/// `points.parquet` with `entity_id`, `x`, `y`, and a `category` utf8 column always null — every
/// binding these fixtures carry is minted through ingest, never through the build.
fn write_points_with_absent_category(path: &Path, n: u64, column: &str) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new(column, DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let values: Vec<Option<String>> = vec![None; n as usize];
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(values)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_args(points: &Path, pairs: &Path, out: &Path, schema: Schema) -> BuildArgs {
    BuildArgs {
        points: points.to_path_buf(),
        corpus: Some(points.to_path_buf()),
        pairs: pairs.to_path_buf(),
        out: out.to_path_buf(),
        extent: extent(),
        view_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        artifacts: None,
        artifact_members: None,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
}

fn build_fixture_with_schema(out: &Path, tmp: &Path, schema_toml: &str, column: &str, n: u64) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points_with_absent_category(&points, n, column);
    write_pairs_n(&pairs, n);
    let schema = parse_schema(tmp, schema_toml);
    build(&build_args(&points, &pairs, out, schema))
        .expect("a discovered-vocabulary build should succeed");
}

fn engine_over(tmp: &Path, root: &Path, config: EngineConfig) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
    .expect("the engine opens against a bundle carrying a discovered vocabulary");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn ingest_row(engine: &Engine, external_id: &str, scalar: WalScalar) -> EntityId {
    engine
        .accept_ingest(
            vec![UnallocatedRow {
                external_id: Some(external_id.as_bytes().to_vec()),
                view: "s0".to_string(),
                descriptors: vec![b"0".to_vec()],
                x: 1.0,
                y: 1.0,
                scalars: vec![scalar],
                terms: engine.resolve_terms(&[b"0".to_vec()]),
            }],
            format!("batch-{external_id}"),
            [0u8; 32],
        )
        .expect("the ingest is accepted")[0]
}

/// One column's stored code for `entity`, read back through `ColumnsRef` by `tessera_id` — the same
/// route the serving path reads it. `None` if the entity has not reached a segment (not flushed).
fn stored_code_of(root: &Path, column: &str, entity: EntityId) -> Option<u32> {
    let bundle = open_bundle(root).expect("the bundle opens");
    let current: tessera_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT is readable"))
            .expect("CURRENT parses");
    let prefix = &current.prefix;
    let tessera_id = test_key().forward(0, entity).unwrap().raw();
    for (phash, partition) in &bundle.partitions {
        for segment in &partition.manifest.segments {
            let dir = root
                .join(prefix)
                .join("partitions")
                .join(phash)
                .join("views")
                .join(&segment.view)
                .join("segments")
                .join(&segment.seg_id);
            let columns = ColumnsRef::load(&dir.join("columns.arrow"))
                .unwrap_or_else(|e| panic!("segment {} must open: {e}", segment.seg_id));
            let ids = columns.tessera_id();
            let Some(row) = ids.iter().position(|&id| id == tessera_id) else {
                continue;
            };
            return match columns.scalar(column) {
                Some(ScalarSlice::U8(v)) => Some(v[row] as u32),
                Some(ScalarSlice::U16(v)) => Some(v[row] as u32),
                Some(ScalarSlice::U32(v)) => Some(v[row]),
                other => {
                    panic!("column '{column}' must be an unsigned category width, got {other:?}")
                }
            };
        }
    }
    None
}

/// Every `VocabularyMint` record in the WAL at `wal_path`, in file order.
fn mint_records(wal_path: &Path) -> Vec<(String, String, u32)> {
    let (_wal, records) = Wal::open(wal_path).expect("the WAL reopens");
    records
        .into_iter()
        .filter_map(|r| match r {
            WalRecord::VocabularyMint {
                vocabulary,
                key,
                code,
            } => Some((vocabulary, key, code)),
            _ => None,
        })
        .collect()
}

// -------------------------------------------------------------------------------------------

/// **Case 1**: a novel category key mints a code, the row's stored scalar is the code — not the
/// key — and the binding is visible on the published generation (in memory) and, after a flush, in
/// the next side-manifest's `vocabulary_extensions` (§3 of the brief: extensions reach the next
/// side-manifest).
#[test]
fn a_novel_key_mints_and_the_row_stores_the_code() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_schema(&root, tmp.path(), DISCOVERED_WIDE, "department", 4);
    let engine = engine_over(tmp.path(), &root, config());

    let entity = ingest_row(&engine, "eng-1", WalScalar::Utf8("eng".to_string()));

    // Visible on the published generation immediately — no flush needed.
    let generation = engine.generation();
    let code = generation
        .vocabularies
        .get("department")
        .expect("the vocabulary is live")
        .code_of("eng")
        .expect("the novel key is bound as soon as the window that minted it is published");
    assert_ne!(code, 0, "0 is the absent sentinel, never a minted code");

    flush(&engine);

    assert_eq!(
        stored_code_of(&root, "department", entity),
        Some(code),
        "the flushed row must carry the code, not the key"
    );

    // §3: the extension reaches the next side-manifest, unioned into whatever it already carries.
    let bundle = open_bundle(&root).expect("the bundle opens");
    let partition = &bundle.partitions["default"];
    let extension = partition
        .manifest
        .vocabulary_extensions
        .iter()
        .find(|e| e.name == "department")
        .expect("the flush's side-manifest carries the 'department' extension");
    assert!(
        extension
            .values
            .iter()
            .any(|v| v.key == "eng" && v.code == code),
        "the extension must carry the minted binding: {:?}",
        extension.values
    );

    drop(engine);
}

/// **Case 2**: two rows in one window carrying the same novel key mint once, get the same code, and
/// produce exactly one `VocabularyMint` record — `mint`'s view-first property, exercised at window
/// scope rather than at the minter's own unit-test scope.
#[test]
fn two_rows_in_one_window_with_the_same_novel_key_mint_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_schema(&root, tmp.path(), DISCOVERED_WIDE, "department", 4);
    let engine = engine_over(tmp.path(), &root, config());

    let row = |external_id: &str| UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 1.0,
        y: 1.0,
        scalars: vec![WalScalar::Utf8("finance".to_string())],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };

    let entities = engine
        .accept_ingest(
            vec![row("f-1"), row("f-2")],
            "batch-finance".to_string(),
            [1u8; 32],
        )
        .expect("one window, two rows, one novel key");
    assert_eq!(entities.len(), 2);

    let generation = engine.generation();
    let code = generation
        .vocabularies
        .get("department")
        .unwrap()
        .code_of("finance")
        .expect("the shared key is bound");

    let wal_path = tmp.path().join("wal.log");
    // Drop first, so the WAL is read as it stands right after the window closed — a flush later
    // would rotate it, reclaiming the very mint record this assertion is about (rotation is what
    // `positions` in `close_window` exists for). `mint_records` and the flush-based checks below
    // therefore run against two different engine handles, in that order.
    drop(engine);
    let finance_mints: Vec<_> = mint_records(&wal_path)
        .into_iter()
        .filter(|(vocabulary, key, _)| vocabulary == "department" && key == "finance")
        .collect();
    assert_eq!(
        finance_mints.len(),
        1,
        "two rows sharing one novel key in one window must produce exactly one mint record, got \
         {finance_mints:?}"
    );
    assert_eq!(finance_mints[0].2, code);

    let engine = engine_over(tmp.path(), &root, config());
    flush(&engine);
    assert_eq!(stored_code_of(&root, "department", entities[0]), Some(code));
    assert_eq!(stored_code_of(&root, "department", entities[1]), Some(code));
    drop(engine);
}

/// **Case 3**: the mint record is appended before the `IngestBatch` record that uses it — asserted
/// on the replayed record order, not merely on both records existing.
#[test]
fn the_mint_record_precedes_the_ingest_batch_record_in_the_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_schema(&root, tmp.path(), DISCOVERED_WIDE, "department", 4);
    let engine = engine_over(tmp.path(), &root, config());

    ingest_row(&engine, "order-1", WalScalar::Utf8("legal".to_string()));

    let wal_path = tmp.path().join("wal.log");
    drop(engine); // joins the executor and closes the WAL handle before it is reopened below

    let (_wal, records) = Wal::open(&wal_path).expect("the WAL reopens");
    let mint_index = records
        .iter()
        .position(|r| {
            matches!(
                r,
                WalRecord::VocabularyMint { vocabulary, key, .. }
                    if vocabulary == "department" && key == "legal"
            )
        })
        .expect("the mint record is in the log");
    let batch_index = records
        .iter()
        .position(
            |r| matches!(r, WalRecord::IngestBatch { batch_id, .. } if batch_id == "batch-order-1"),
        )
        .expect("the ingest batch record is in the log");
    assert!(
        mint_index < batch_index,
        "the mint (index {mint_index}) must precede the batch that uses its code (index \
         {batch_index})"
    );
}

/// **Case 4**: after a restart, a minted code is unchanged and is never re-drawn for a different
/// key — the never-reuse invariant surviving replay, not merely the binding surviving it.
///
/// Constructed so a broken seed cannot hide behind luck: the fixture retires 250 of a `u8`'s 255
/// usable codes at build (`discovered_sparse_schema_toml`), so exactly five codes are free before
/// the first ingest. One key takes one of those five before the restart; if replay failed to carry
/// its code into the reopened minter's assigned set, one of the four *subsequent* draws would reuse
/// it — a near certainty, not a one-in-65535 coincidence a wider vocabulary would leave to chance.
#[test]
fn a_minted_code_survives_a_restart_and_is_never_redrawn() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    let schema_toml = discovered_sparse_schema_toml();
    build_fixture_with_schema(&root, tmp.path(), &schema_toml, "department", 2);

    let engine = engine_over(tmp.path(), &root, config());
    ingest_row(
        &engine,
        "before-restart",
        WalScalar::Utf8("eng".to_string()),
    );
    let code_before = engine
        .generation()
        .vocabularies
        .get("department")
        .unwrap()
        .code_of("eng")
        .expect("eng is bound before the restart");
    assert!(
        (251..=255).contains(&code_before),
        "only five codes are free before this ingest: {code_before}"
    );
    drop(engine); // joins the executor and closes the WAL before the reopen below

    let engine = engine_over(tmp.path(), &root, config());
    let code_after = engine
        .generation()
        .vocabularies
        .get("department")
        .unwrap()
        .code_of("eng")
        .expect("the binding must survive replay");
    assert_eq!(
        code_before, code_after,
        "a pinned code must not move across a restart"
    );

    // Exactly four free codes remain (251..=255 minus `code_before`). Four more novel keys must
    // mint, none of them reusing `code_before`, and a fifth must be refused as exhausted.
    let mut minted = Vec::new();
    for i in 0..4 {
        let key = format!("post-restart-{i}");
        ingest_row(&engine, &key, WalScalar::Utf8(key.clone()));
        let code = engine
            .generation()
            .vocabularies
            .get("department")
            .unwrap()
            .code_of(&key)
            .expect("each novel key mints");
        assert_ne!(
            code, code_before,
            "a code minted before the restart must never be redrawn for a different key"
        );
        minted.push(code);
    }
    let mut dedup = minted.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(
        dedup.len(),
        4,
        "no two of the four post-restart keys may share a code"
    );

    // The space is now fully spent (`code_before` plus the four just minted = all five free
    // codes); a sixth novel key must be refused, not silently reuse one of the five.
    let err = engine
        .accept_ingest(
            vec![UnallocatedRow {
                external_id: Some(b"one-too-many".to_vec()),
                view: "s0".to_string(),
                descriptors: vec![b"0".to_vec()],
                x: 1.0,
                y: 1.0,
                scalars: vec![WalScalar::Utf8("one-too-many".to_string())],
                terms: engine.resolve_terms(&[b"0".to_vec()]),
            }],
            "batch-one-too-many".to_string(),
            [9u8; 32],
        )
        .expect_err("the department vocabulary's code space is now exhausted");
    let AcceptError::Exec(exec_error) = err else {
        panic!("exhaustion must reach the caller as an executor failure, got {err}");
    };
    let detail = exec_error.to_string();
    assert!(
        detail.contains("department") && detail.contains("full"),
        "the detail must name the exhausted vocabulary: {detail}"
    );
}

/// **Case 5**: a declared vocabulary is unaffected. Its column never carries `WalScalar::Utf8` at
/// this boundary — the handler either resolves a known key to its code or refuses an unknown one
/// with a 422 before the executor ever sees it (§5's declare-then-use rule) — so the executor's mint
/// loop, which acts only on a `WalScalar::Utf8` beside a `vocabulary` declaration, has nothing to do
/// for it: the stored code is exactly the one ingested, and no binding is created.
#[test]
fn a_declared_vocabulary_is_unaffected_by_the_mint_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_schema(&root, tmp.path(), DECLARED_BAND, "band", 2);
    let engine = engine_over(tmp.path(), &root, config());

    let assigned_before = engine
        .generation()
        .vocabularies
        .get("band")
        .unwrap()
        .assigned_count();
    assert_eq!(assigned_before, 3, "low/mid/high, declared at build");

    // A declared column arrives already coded — never as `WalScalar::Utf8` — because the handler
    // resolves or refuses it before the executor is reached; this row states that directly.
    let entity = ingest_row(&engine, "declared-1", WalScalar::U8(2));

    let assigned_after = engine
        .generation()
        .vocabularies
        .get("band")
        .unwrap()
        .assigned_count();
    assert_eq!(
        assigned_after, assigned_before,
        "nothing mints for a declared vocabulary — the assigned set must not grow"
    );

    flush(&engine);
    assert_eq!(
        stored_code_of(&root, "band", entity),
        Some(2),
        "an already-coded declared value must reach the segment unchanged"
    );
}
