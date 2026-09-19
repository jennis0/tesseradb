//! **Term images are the same feature whether the corpus was built or ingested** (decision 0091).
//!
//! One corpus reaches a bundle two ways. The first is a build over a points file and an access
//! relation. The second is a build with no points at all, into which the same corpus arrives
//! through `/control/ingest` with each row naming its own access descriptors, followed by a flush
//! and a fold. The fold is where an ingest-only deployment gets its images (ruling 5, decision
//! 0143), so until it runs the second bundle has none.
//!
//! What is compared is what a principal is served, and which terms have an image. Both are
//! compared against the corpus's own definition and not only between the two bundles, so two paths
//! agreeing on a wrong answer would still fail.
//!
//! **Through the external id, reached from the `tessera_id` each response carries.** The two
//! bundles assign different entity ids to the same item: a build assigns in term-signature order
//! and an ingest assigns in arrival order, and `tessera_id` is a permutation of the entity id, so
//! the two responses cannot be compared as identities. The external id is the caller's own name
//! for the item and is the same on both sides, so each served `tessera_id` is resolved through
//! `Engine::item` and the sets of names compared.
//!
//! Term ids differ for the same reason: a build interns the corpus's descriptors in its own order
//! and an ingest mints them as rows arrive. So the kept-term sets are compared by descriptor
//! string, each side resolving the string against its own dictionary.

mod common;

use std::collections::BTreeSet;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::UnallocatedRow;
use tessera_types::TesseraId;

const WAIT: Duration = Duration::from_secs(180);

/// The one view both paths carry.
const VIEW: &str = "s0";

/// Entities in the corpus. Small enough that the ingest path's rows all arrive inside one test,
/// and large enough that a term can hold more than the keep rule's thirty rows per container.
const ENTITIES: u64 = 4_096;

/// Rows per ingest batch.
const BATCH: usize = 256;

// The corpus's access terms, by the descriptor a credential and an ingest row name them with.
/// Every second entity. Above the keep rule, so it has an image.
const HALF: u32 = 11;
/// Every tenth. Above the keep rule.
const TENTH: u32 = 14;
/// Every hundredth, forty-one entities. Above the keep rule, and the narrowest term that is.
const HUNDREDTH: u32 = 15;
/// The first of the terms too small to project at all. Term `TINY_BASE + n` holds `1 + n % 20`
/// entities from `n × 100`, so none of them reaches the thirty-entity skip and every one of them
/// is residual.
const TINY_BASE: u32 = 100;
/// How many of those the corpus carries.
const TINY_TERMS: u32 = 8;

/// Which terms entity `i` carries.
fn terms_of(i: u64) -> Vec<u32> {
    let mut terms = Vec::new();
    if i.is_multiple_of(2) {
        terms.push(HALF);
    }
    if i % 10 == 3 {
        terms.push(TENTH);
    }
    if i % 100 == 7 {
        terms.push(HUNDREDTH);
    }
    let n = i / 100;
    if n < u64::from(TINY_TERMS) && i - n * 100 < 1 + n % 20 {
        terms.push(TINY_BASE + n as u32);
    }
    terms.sort_unstable();
    terms
}

/// Every descriptor the corpus uses, which is what the kept-term comparison runs over.
fn every_term() -> Vec<u32> {
    let mut terms = vec![HALF, TENTH, HUNDREDTH];
    terms.extend(TINY_BASE..TINY_BASE + TINY_TERMS);
    terms
}

/// A descriptor as a credential and an ingest row spell it.
fn label(term: u32) -> Vec<u8> {
    term.to_string().into_bytes()
}

/// The credential naming `terms`.
fn credential(terms: &[u32]) -> Vec<u8> {
    let named: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
    format!("{{\"terms\": [{}]}}", named.join(", ")).into_bytes()
}

/// The external id both paths name entity `i` by: the eight little-endian bytes a build mints from
/// an integer identity column, which is what the ingest supplies for itself.
fn external_id(i: u64) -> Vec<u8> {
    i.to_le_bytes().to_vec()
}

fn x_of(i: u64) -> f64 {
    ((i * 37) % 1000) as f64
}

fn y_of(i: u64) -> f64 {
    ((i * 53) % 1000) as f64
}

/// The principals the two bundles are compared over. Each holds a term with an image and a term
/// without one, except the first, which holds only terms too small to project.
fn principals() -> Vec<(&'static str, Vec<u32>)> {
    vec![
        ("tiny", vec![TINY_BASE, TINY_BASE + 1]),
        ("1%", vec![HUNDREDTH, TINY_BASE + 2]),
        ("10%", vec![TENTH, TINY_BASE + 3]),
        ("50%", vec![HALF, HUNDREDTH]),
    ]
}

/// The external ids a principal holding `terms` is entitled to, from the corpus's own definition.
///
/// The comparison below is against this rather than only between the two bundles: two paths that
/// agreed on the wrong set would otherwise pass.
fn entitled(terms: &[u32]) -> BTreeSet<Vec<u8>> {
    (0..ENTITIES)
        .filter(|i| terms_of(*i).iter().any(|term| terms.contains(term)))
        .map(external_id)
        .collect()
}

// -------------------------------------------------------------------------------------------
// The two bundles
// -------------------------------------------------------------------------------------------

fn write_points(path: &Path, entities: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..entities).collect();
    let xs: Vec<f64> = ids.iter().copied().map(x_of).collect();
    let ys: Vec<f64> = ids.iter().copied().map(y_of).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn write_pairs(path: &Path, entities: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut ids = Vec::new();
    let mut terms = Vec::new();
    for entity in 0..entities {
        for term in terms_of(entity) {
            ids.push(entity);
            terms.push(term);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// Build a bundle over `entities` of the corpus. `0` is the empty database the ingest path starts
/// from: the same declaration, the same frame, and no row.
fn build_bundle(dir: &Path, name: &str, entities: u64) -> std::path::PathBuf {
    let points = dir.join(format!("{name}-points.parquet"));
    let pairs = dir.join(format!("{name}-pairs.parquet"));
    write_points(&points, entities);
    write_pairs(&pairs, entities);
    let out = dir.join(name);
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: VIEW.to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    build(&args).expect("the bundle builds");
    out
}

/// θ saturated and the caps above what any tile holds, so what a response carries is the
/// principal's whole visible set and not a sample of it. A live density rule would put a second
/// arithmetic between the two bundles and every comparison below.
fn ingest_config() -> EngineConfig {
    EngineConfig {
        theta_target_marks: ENTITIES * 2,
        max_k: ENTITIES as usize * 2,
        k_max_marks: ENTITIES as usize * 2,
        flush_max_age_secs: 3600,
        flush_max_items: usize::MAX,
        ..config_uncapped()
    }
}

fn open(root: &Path, dir: &Path, name: &str) -> Engine {
    Engine::open(
        root,
        &dir.join(format!("cache-{name}")),
        &dir.join(format!("{name}.log")),
        tessera_plugin::Passthrough::new(),
        ingest_config(),
    )
    .expect("the engine opens")
}

/// Send the whole corpus through `/control/ingest`, in batches, each row naming its own
/// descriptors.
fn ingest_corpus(engine: &Engine) {
    for start in (0..ENTITIES).step_by(BATCH) {
        let end = (start + BATCH as u64).min(ENTITIES);
        let rows: Vec<UnallocatedRow> = (start..end)
            .map(|i| {
                let descriptors: Vec<Vec<u8>> = terms_of(i).into_iter().map(label).collect();
                UnallocatedRow {
                    external_id: Some(external_id(i)),
                    view: VIEW.to_string(),
                    join: None,
                    x: x_of(i),
                    y: y_of(i),
                    scalars: Vec::new(),
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                    scoped: Vec::new(),
                }
            })
            .collect();
        engine
            .accept_ingest(rows, format!("batch-{start}"), [0u8; 32])
            .expect("the batch is accepted");
    }
}

/// Fold, and return when the fold has published.
fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(Instant::now() < deadline, "the fold never published");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// -------------------------------------------------------------------------------------------
// What is compared
// -------------------------------------------------------------------------------------------

/// Authorise, waiting out a concurrent build of the same credential's fragment (lifecycle §3.3).
fn authorise(engine: &Engine, credential: &[u8]) -> tessera_engine::Session {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match engine.authorise(credential) {
            Ok(session) => return session,
            Err(tessera_engine::EngineError::FragmentBuilding) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("the credential must authorise: {error}"),
        }
    }
}

/// The external ids one principal is served over the whole extent, reached from the `tessera_id`
/// of each served point.
fn served_ids(engine: &Engine, credential: &[u8]) -> BTreeSet<Vec<u8>> {
    let session = authorise(engine, credential);
    let response = engine
        .viewport(
            &session,
            ViewportRequest::new(VIEW, 2, [0.0, 0.0, 1000.0, 1000.0], ENTITIES as usize * 2),
        )
        .expect("the viewport serves");
    response
        .points
        .tessera_ids
        .iter()
        .map(|id| {
            engine
                .item(&session, TesseraId::new(*id), None)
                .expect("the drill-down answers")
                .expect("a served point is an item this principal may reach")
                .external_id
                .expect("every item in this corpus carries the external id the caller supplied")
        })
        .collect()
}

/// Which of the corpus's descriptors have an image in `engine`'s view, by descriptor string.
///
/// Each side resolves the string against its own dictionary, because the two interned the corpus
/// in different orders and a term id means nothing across them.
fn kept_descriptors(engine: &Engine, where_: &str) -> BTreeSet<String> {
    let generation = engine.generation();
    let view = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(VIEW))
        .expect("the generation carries the view");
    let images = view
        .term_images
        .as_ref()
        .unwrap_or_else(|| panic!("{where_} has no image table, so there is nothing to compare"));
    every_term()
        .into_iter()
        .filter(|term| {
            let id = engine.resolve_terms(&[label(*term)])[0];
            images.kept(id)
        })
        .map(|term| term.to_string())
        .collect()
}

/// **The built bundle and the ingested one serve the same corpus and keep images of the same
/// terms.**
///
/// The ingest path starts from a build with no points, takes the corpus in through
/// `/control/ingest`, flushes and folds. Before the fold it has no images at all, which is asserted
/// on the way through: an ingest-only deployment is served by the walk until its first fold, so a
/// comparison made before it would establish nothing about images.
#[test]
fn a_built_corpus_and_the_same_corpus_ingested_and_folded_agree() {
    let tmp = tempfile::TempDir::new().unwrap();

    let built_root = build_bundle(tmp.path(), "built", ENTITIES);
    let built = open(&built_root, tmp.path(), "built");

    let empty_root = build_bundle(tmp.path(), "empty", 0);
    let mut ingested = open(&empty_root, tmp.path(), "ingested");
    ingested
        .start_write_executor(64)
        .expect("the executor starts once");
    ingest_corpus(&ingested);

    // Before the fold: the flush's rows are served, and no image exists to serve them from.
    let flushes = ingested.write_executor_stats().flushes;
    ingested.request_flush();
    wait_until("the first flush to publish", WAIT, || {
        ingested.write_executor_stats().flushes > flushes
    });
    {
        let generation = ingested.generation();
        let view = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(VIEW))
            .expect("the ingested generation carries the view");
        assert!(
            view.term_images.is_none(),
            "an ingest-only deployment has no images until its first fold"
        );
    }

    fold(&ingested);

    let kept = kept_descriptors(&built, "the built bundle");
    assert_eq!(
        kept,
        kept_descriptors(&ingested, "the ingested and folded bundle"),
        "the two paths kept images of different terms"
    );
    // The corpus's own classification, checked rather than assumed: the three terms above the
    // keep rule have images and the eight below the skip do not. Without it the equality above
    // would pass over two empty sets.
    let expected: BTreeSet<String> = [HALF, TENTH, HUNDREDTH]
        .into_iter()
        .map(|term| term.to_string())
        .collect();
    assert_eq!(
        kept, expected,
        "the corpus must keep the three terms above the keep rule and none of the small ones"
    );

    for (name, terms) in principals() {
        let credential = credential(&terms);
        let expected = entitled(&terms);
        assert!(
            !expected.is_empty(),
            "principal {name} is entitled to nothing, so the comparison says nothing"
        );
        assert_eq!(
            served_ids(&built, &credential),
            expected,
            "the built bundle serves principal {name} a set that is not the corpus's"
        );
        assert_eq!(
            served_ids(&ingested, &credential),
            expected,
            "the ingested and folded bundle serves principal {name} a set that is not the \
             corpus's"
        );
    }
}
