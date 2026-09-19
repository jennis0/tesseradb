//! **A freshly built bundle serves in the form its own statistics choose, and derives nothing to
//! do it.**
//!
//! Two defects, both from `docs/evidence/memos/2026-08-22-artifact-scale-campaign.md`, and one
//! fixture:
//!
//! - **Finding 1** — the build recorded `ArtifactMajor` for levels its own reported figures said
//!   should be row-major, and only the first fold re-evaluated. The cost was 10–11× on the
//!   campaign's 10⁷ tier, paid by every deployment between its build and its first fold.
//! - **Finding 2 ⊘** — because the build wrote none of the derived structures, the first
//!   `/v1/viewport` naming such a level built the row form *inside the response* and was truncated
//!   at the 60-second whole-stream deadline, silently. Reproduced there at 9.83M artifacts and
//!   twice more at 5×10⁵, so the trigger is a row-form build over about half a million artifacts.
//!
//! The shape is the campaign's, at a scale a test can hold: an enumerated layer of more than a
//! thousand artifacts whose memberships **span the corpus**, so no node of the tile index holds one
//! and the `everywhere` fraction — the layout trigger — is 1.0.
//!
//! **What this asserts is not "it is fast".** A wall-clock bound over a small fixture proves
//! nothing about a large one. What it asserts is *the thing that was slow does not happen*: the
//! first request **adopts** the mapped column and index the build wrote and **composes** neither,
//! read off the engine's own gauges. The deadline the campaign's request died on was 60 s; the
//! bound here is generous by two orders and exists only to catch a regression that reintroduces the
//! derivation on the request path.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use tessera_build::BuildArgs;
use tessera_engine::{LayerSelection, ViewportRequest};
use tessera_types::layer::ServingLayout;

const SPREAD: &str = "clusters/spread";
const CLUMPED: &str = "clusters/clumped";
const N: u64 = 12_000;
/// Above `ROW_MAJOR_MIN_ARTIFACTS`, which is the count tiebreak the spread has to clear.
const SPREAD_ARTIFACTS: u64 = 1_200;
/// Well under it, so the clumped layer's own case is about the tiebreak and not about the spread.
const CLUMPED_ARTIFACTS: u64 = 60;

const CONFIG_TOML: &str = r#"
[sources]
spread          = "spread.parquet"
spread_members  = "spread_members.parquet"
clumped         = "clumped.parquet"
clumped_members = "clumped_members.parquet"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[layer]]
name = "clusters/spread"
views = ["s0"]
source = "spread"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }

  [layer.members]
  source = "spread_members"

[[layer]]
name = "clusters/clumped"
views = ["s0"]
source = "clumped"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }

  [layer.members]
  source = "clumped_members"
"#;

fn write(path: &Path, schema: Arc<Schema>, batch: RecordBatch) {
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_keys(path: &Path, keys: &[String]) {
    let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(keys.to_vec())) as ArrayRef],
    )
    .unwrap();
    write(path, schema, batch);
}

fn write_members(path: &Path, rows: &[(String, u64)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("rank", DataType::UInt32, true),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(UInt32Array::from(vec![None::<u32>; rows.len()])),
            Arc::new(UInt64Array::from(
                rows.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// **The spread layer, and why it partitions.** Artifact *i* takes every source id congruent to
/// *i* modulo the artifact count, so each membership is a stride across the whole corpus — no
/// index node contains one — and the memberships are disjoint by construction, which is what makes
/// the pick land on `RowMajorLabel` rather than `RowMajorList`.
///
/// **A source id is not a row.** The tiler sorts by Morton code, so a stride in source ids is not
/// a stride in rows for any reason the fixture controls — which is the point: the extent that
/// results is observed, and the assertion below reads it off the shape the build reports.
fn spread_members() -> (Vec<String>, Vec<(String, u64)>) {
    let keys: Vec<String> = (0..SPREAD_ARTIFACTS).map(|i| format!("s-{i:05}")).collect();
    let mut rows = Vec::with_capacity(N as usize);
    for entity in 0..N {
        rows.push((keys[(entity % SPREAD_ARTIFACTS) as usize].clone(), entity));
    }
    (keys, rows)
}

/// The clumped layer: contiguous blocks of source ids, and few enough artifacts to sit under the
/// count tiebreak either way.
fn clumped_members() -> (Vec<String>, Vec<(String, u64)>) {
    let keys: Vec<String> = (0..CLUMPED_ARTIFACTS)
        .map(|i| format!("c-{i:05}"))
        .collect();
    let block = N / CLUMPED_ARTIFACTS;
    let mut rows = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        for entity in (i as u64 * block)..((i as u64 + 1) * block) {
            rows.push((key.clone(), entity));
        }
    }
    (keys, rows)
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let config_path = tmp.path().join("config.toml");
    write_points_n(&points, N);
    write_pairs_n(&pairs, N);
    std::fs::write(&config_path, CONFIG_TOML).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture config parses");

    let (spread_keys, spread_rows) = spread_members();
    write_keys(&tmp.path().join("spread.parquet"), &spread_keys);
    write_members(&tmp.path().join("spread_members.parquet"), &spread_rows);
    let (clumped_keys, clumped_rows) = clumped_members();
    write_keys(&tmp.path().join("clumped.parquet"), &clumped_keys);
    write_members(&tmp.path().join("clumped_members.parquet"), &clumped_rows);

    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: root.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    tessera_build::build(&args).expect("a build carrying two enumerated layers");
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

fn manifest(fx: &Fixture) -> tessera_store::manifest::SegmentsManifest {
    let engine = open_engine(&fx.root, &fx.cache.join("m"), &fx.wal.with_extension("m"));
    let generation = engine.generation();
    generation
        .bundle
        .partitions
        .values()
        .next()
        .expect("one partition")
        .manifest
        .clone()
}

/// **Finding 1's fix.** The build's own pass chooses each level's layout from the bundle's row
/// space and records it — so a level the walk cannot place is `RowMajorLabel` in the manifest a
/// build wrote, with no fold anywhere.
#[test]
fn the_build_records_the_layout_its_own_row_space_chooses() {
    let fx = fixture();
    let engine = open_engine(&fx.root, &fx.cache, &fx.wal);
    let generation = engine.generation();
    let layers = &generation
        .bundle
        .partitions
        .values()
        .next()
        .unwrap()
        .manifest
        .layers;

    let spread = layers
        .iter()
        .find(|l| l.declaration.name == SPREAD)
        .expect("the spread layer is registered");
    assert_eq!(
        spread.layout_of(0),
        ServingLayout::RowMajorLabel,
        "a level of {SPREAD_ARTIFACTS} disjoint memberships spanning the corpus is row-major, and \
         the build is what says so"
    );

    // The tiebreak, in the same bundle: the same evidence under a thousand artifacts changes
    // nothing, which is what makes the flip above about the level and not about the pass running.
    let clumped = layers
        .iter()
        .find(|l| l.declaration.name == CLUMPED)
        .expect("the clumped layer is registered");
    assert_eq!(
        clumped.layout_of(0),
        ServingLayout::ArtifactMajor,
        "{CLUMPED_ARTIFACTS} artifacts is below the count tiebreak whatever their spread"
    );
}

/// The manifest names the files the pass wrote, on coordinates a reader can adopt: a column for
/// the level that flipped, an index for the level that did not, and neither for the other.
#[test]
fn the_manifest_names_a_column_for_one_level_and_an_index_for_the_other() {
    let fx = fixture();
    let manifest = manifest(&fx);

    let is_column = |e: &&tessera_store::manifest::DerivedExtent| matches!(e.form, tessera_store::manifest::DerivedForm::RowColumn { .. });
    let is_index = |e: &&tessera_store::manifest::DerivedExtent| e.form == tessera_store::manifest::DerivedForm::TileIndex;
    let columns: Vec<_> = manifest
        .derived_extents
        .iter()
        .filter(is_column)
        .filter(|e| e.layer == SPREAD)
        .collect();
    assert_eq!(columns.len(), 1, "one column per (view, layer, level)");
    assert_eq!(columns[0].view.as_deref(), Some("s0"));
    assert_eq!(
        columns[0].form,
        tessera_store::manifest::DerivedForm::RowColumn {
            layout: ServingLayout::RowMajorLabel
        }
    );
    assert!(
        manifest
            .derived_extents
            .iter()
            .filter(is_column)
            .all(|e| e.layer != CLUMPED),
        "an artifact-major level has no column"
    );

    let indexes: Vec<_> = manifest
        .derived_extents
        .iter()
        .filter(is_index)
        .filter(|e| e.layer == CLUMPED)
        .collect();
    assert_eq!(indexes.len(), 1, "one index per (view, layer, level)");
    assert!(
        manifest
            .derived_extents
            .iter()
            .filter(is_index)
            .all(|e| e.layer != SPREAD),
        "a row-major level has nothing to index — its candidacy is a scan of the viewport"
    );

    // And every file the manifest names is on disk under the prefix and digested in
    // `MANIFEST.files` — the rule that makes a torn write attributable rather than silent.
    let engine = open_engine(&fx.root, &fx.cache.join("f"), &fx.wal.with_extension("f"));
    let generation = engine.generation();
    let prefix_dir = fx.root.join(generation.prefix.as_str());
    let digests = &generation.bundle.manifest.files;
    for path in manifest.derived_extents.iter().map(|e| &e.path) {
        assert!(
            prefix_dir.join(path).exists(),
            "{path} is named by the manifest and is not in the prefix"
        );
        assert!(
            digests.contains_key(path.as_str()),
            "{path} is named by the manifest and is not digested"
        );
    }
}

/// **Finding 2's fix, and the assertion that matters.** A fresh bundle's first viewport adopts the
/// mapped forms and composes nothing — so the minute-long derivation the campaign found running
/// *inside* a response does not run at all.
#[test]
fn the_first_request_over_a_fresh_bundle_adopts_and_composes_nothing() {
    let fx = fixture();
    let engine = open_engine(&fx.root, &fx.cache, &fx.wal);

    // Nothing has been composed, because nothing has been asked for yet.
    assert_eq!(engine.columns_composed(), 0);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let started = std::time::Instant::now();
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, 0)
                .layers(LayerSelection::Named(&[SPREAD, CLUMPED])),
        )
        .expect("the first viewport over a freshly built bundle");
    let elapsed = started.elapsed();

    assert_eq!(
        out.artifacts.iter().filter(|a| a.layer == SPREAD).count(),
        SPREAD_ARTIFACTS as usize,
        "every artifact of the spread layer is served to a principal that sees the corpus"
    );
    // **Both gauges are counted at the claim rather than at the open**, deliberately: an entry the
    // manifest named and no request ever asked for saved nothing. So they are read after the first
    // request, which is exactly the request the campaign found deriving these instead.
    assert!(
        engine.columns_adopted() > 0,
        "the build wrote a row-major column and the first request did not claim it"
    );
    assert_eq!(
        engine.columns_composed(),
        0,
        "the first request composed a row-major column the build had already written — this is \
         the cold-path derivation the 60-second stream deadline fired on"
    );
    assert!(
        engine.artifact_tile_indexes_adopted() > 0,
        "the clumped level's index was written by the build and should be claimed, not derived"
    );
    // Two orders inside the deadline the campaign's request died on. A bound, not a benchmark.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "the first request took {elapsed:?}"
    );
}

/// The counts a fresh bundle serves are the counts a fold-settled one serves. **The layout decides
/// which structure is walked and never what the answer is**, so the level that flipped at the
/// build has to agree with the same level served artifact-major — which is what a pin gives us a
/// way to ask.
#[test]
fn the_flipped_level_answers_what_the_artifact_major_route_answers() {
    let fx = fixture();
    let engine = open_engine(&fx.root, &fx.cache, &fx.wal);
    let session = engine.authorise(&subset_credential()).unwrap();
    let row_major = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, 0).layers(LayerSelection::Named(&[SPREAD])),
        )
        .expect("a viewport over the row-major level")
        .artifacts;

    // The same question of the level that stayed artifact-major, as a control: both routes serve
    // every artifact holding a member this principal can see, and neither serves one that does not.
    let artifact_major = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, 0).layers(LayerSelection::Named(&[CLUMPED])),
        )
        .expect("a viewport over the artifact-major level")
        .artifacts;

    let row_major_total: u64 = row_major.iter().map(|a| a.masked_count).sum();
    let artifact_major_total: u64 = artifact_major.iter().map(|a| a.masked_count).sum();
    assert!(row_major_total > 0);
    assert_eq!(
        row_major_total, artifact_major_total,
        "the two layers partition the same corpus, so a principal's visible members sum the same \
         over either — whichever route each level is served by"
    );
}
