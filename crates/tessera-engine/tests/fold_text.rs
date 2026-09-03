//! **A text column across the seam: base index, flush extent, and the fold that merges them.**
//!
//! `tests/deletion_reaches_every_home.rs` covers the fold's text pass against the base layer
//! alone — build, delete, fold. What it cannot reach is the shape the pass exists for: a corpus
//! whose words live in *several* layers at once, because every flush since the last fold minted a
//! dictionary of its own and the ordinals in one name nothing in another. That is where a merge
//! can go wrong quietly — a term present in two layers counted once, a posting read against the
//! neighbouring layer's word — and none of it surfaces as an error.
//!
//! So the three cases here all run the same arc: build a corpus with prose, ingest more, let the
//! flush publish its own text layer, then fold and ask the merged artefact the questions. What
//! separates them is which property is under test — that the merge is complete, that it subtracts
//! the deleted set, and that the fold's own bookkeeping leaves nothing behind.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::Engine;
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::UnallocatedRow;
use tessera_store::read::open_bundle;
use tessera_types::{AttrLocalId, EntityId};

/// Built items. Small: every assertion here is exhaustive over the corpus and the fold is the
/// expensive step.
const N: u64 = 32;

const SCHEMA_TOML: &str = r#"
[[attribute]]
name     = "prose"
type     = "text"
index    = true
analyser = "unicode"
"#;

/// The built corpus's prose. `corpus` is carried by every item, `evenword`/`oddword` by half each,
/// and item 7 alone carries `builtsole` — the word a deletion has to take out of the dictionary
/// altogether rather than leave standing with an empty posting.
fn built_prose(source: u64) -> String {
    let parity = if source.is_multiple_of(2) {
        "evenword"
    } else {
        "oddword"
    };
    if source == 7 {
        return format!("corpus {parity} builtsole");
    }
    format!("corpus {parity}")
}

/// The ingested items' prose. `corpus` again — the word that must **not** be double-counted or
/// lost when two layers both hold it — plus `freshword` which only the flushed layer has, and
/// `flushsole` on exactly one of them.
fn flushed_prose(i: u64) -> String {
    if i == 1 {
        return "corpus freshword flushsole".to_string();
    }
    "corpus freshword".to_string()
}

const FLUSHED: u64 = 4;

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("prose", DataType::Utf8, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 37) % 500) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 53) % 500) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| built_prose(*e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities: Vec<u64> = Vec::new();
    let mut terms: Vec<u32> = Vec::new();
    for e in 0..n {
        for t in terms_of(e) {
            entities.push(e);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_text_fixture(out: &Path, tmp: &Path) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points(&points, N);
    write_pairs(&pairs, N);
    let schema_path = tmp.join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &HashMap::new())
        .expect("the text schema parses")
        .schema;
    build(&BuildArgs {
        arena_order: Default::default(),
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
        attribute_sources: tessera_build::config::AttributeSource::over(points.clone(), &schema),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("a bundle with an indexed text column builds");
}

fn engine_over(tmp: &Path, root: &Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the engine opens against the text fixture");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

/// Ingest [`FLUSHED`] items carrying prose, and block until the flush has published their text
/// layer — which is what makes the fold below a *merge* rather than a rewrite of one layer.
fn ingest_and_flush(engine: &Engine, root: &Path) -> Vec<EntityId> {
    let mut out = Vec::new();
    for i in 0..FLUSHED {
        let external = format!("fresh-{i}");
        let row = UnallocatedRow {
            external_id: Some(external.as_bytes().to_vec()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"0".to_vec()],
            x: 10.0 + i as f64,
            y: 10.0 + i as f64,
            scalars: vec![WalScalar::Utf8(flushed_prose(i))],
            terms: engine.resolve_terms(&[b"0".to_vec()]),
            scoped: Vec::new(),
        };
        out.push(
            engine
                .accept_ingest(vec![row], external, [0u8; 32])
                .expect("an ingest carrying prose is accepted")[0],
        );
    }
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if !text_extents(root).is_empty() {
            return out;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published a text extent"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn current_prefix(root: &Path) -> String {
    let current: tessera_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT is readable"))
            .expect("CURRENT parses");
    current.prefix
}

fn partition_dir(root: &Path) -> PathBuf {
    root.join(current_prefix(root)).join("partitions/default")
}

fn text_extents(root: &Path) -> Vec<tessera_store::manifest::TextExtent> {
    open_bundle(root).expect("the bundle opens").partitions["default"]
        .manifest
        .text_extents
        .clone()
}

/// Every term the **base** index holds, with the entities its posting names.
///
/// The base alone, deliberately: after a fold there is nothing else, and reading through the
/// composed layers instead would let a fold that merged nothing and carried every extent forward
/// answer these questions correctly.
fn base_index(root: &Path) -> BTreeMap<String, croaring::Bitmap> {
    let dir = partition_dir(root).join("attrs/prose");
    let dict = tessera_filter::SortedDict::open_dir(&dir, tessera_filter::Access::Read)
        .expect("the merged token dictionary opens");
    let postings = tessera_filter::ColumnPostings::open(&dir.join("postings.arrow"), false)
        .expect("the merged token postings open");
    assert_eq!(
        dict.len(),
        postings.record_count(),
        "the merged dictionary and its postings disagree about how many terms there are"
    );
    let mut out = BTreeMap::new();
    dict.walk(|ordinal, key| {
        out.insert(
            key.to_string(),
            postings
                .entities(AttrLocalId::new(ordinal))
                .expect("a posting reads"),
        );
    })
    .expect("the merged dictionary walks");
    out
}

/// What the corpus's prose says the index should hold, given a set of entities to leave out.
fn expected_index(
    built: &BTreeMap<u64, u64>,
    flushed: &[EntityId],
    without: &[u32],
) -> BTreeMap<String, croaring::Bitmap> {
    let mut out: BTreeMap<String, croaring::Bitmap> = BTreeMap::new();
    let mut add = |prose: String, entity: u32| {
        if without.contains(&entity) {
            return;
        }
        for word in prose.split(' ') {
            out.entry(word.to_string()).or_default().add(entity);
        }
    };
    for (source, entity) in built {
        add(built_prose(*source), *entity as u32);
    }
    for (i, entity) in flushed.iter().enumerate() {
        add(flushed_prose(i as u64), entity.raw() as u32);
    }
    out
}

// ---------------------------------------------------------------------------------------------

/// **The merge is complete: every word of every layer, and a word in two layers is one term.**
///
/// No deletion at all, so what this isolates is the merge itself. `corpus` is in both the base
/// dictionary and the flush's, and after the fold there must be exactly one term for it naming
/// every entity from both — a merge that emitted it twice leaves a dictionary the writer would
/// refuse, and one that took the first layer's posting alone silently loses the flushed batch.
///
/// **Mutations this kills:** advancing only the least layer's cursor (`corpus` names the base's
/// entities alone); taking the last layer to hold a term rather than the union (the base's are
/// lost); dropping the extents from the plan (`freshword` is absent entirely).
#[test]
fn the_fold_merges_every_layers_words_into_one_index() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_text_fixture(&root, tmp.path());
    let engine = engine_over(tmp.path(), &root);
    let built = source_to_new_map(&root, "v00000");

    let flushed = ingest_and_flush(&engine, &root);
    assert_eq!(
        text_extents(&root).len(),
        1,
        "the flush published exactly one text layer, which is what the fold has to merge"
    );
    // Before the fold the base holds the built words and none of the flushed ones — otherwise
    // "the fold merged them" would be satisfied by a fixture that had them there all along.
    let before = base_index(&root);
    assert!(before.contains_key("evenword"));
    assert!(
        !before.contains_key("freshword"),
        "the flushed batch's words are in its own layer, not the base"
    );

    fold(&engine);

    let index = base_index(&root);
    let expected = expected_index(&built, &flushed, &[]);
    assert_eq!(
        index.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "the merged dictionary is not the union of the layers' words"
    );
    for (word, carriers) in &expected {
        assert_eq!(&index[word], carriers, "'{word}' names the wrong entities");
    }
    // The word both layers carried, stated on its own because it is the case a k-way merge gets
    // wrong: one term, and its posting spans the two layers.
    assert_eq!(
        index["corpus"].cardinality(),
        N + FLUSHED,
        "'corpus' is in both layers and its merged posting must hold every entity from both"
    );
}

/// **The merge subtracts the deleted set, whichever layer the entity was in.**
///
/// One deletion from the built corpus and one from the flushed batch, each the sole carrier of a
/// word — so each must lose its posting entry *and* take its word out of the dictionary. A pass
/// that blanked only the base's postings leaves the flushed item's words standing, which is Rule F
/// broken for exactly the entities a fold is most likely to mishandle.
///
/// **Mutations this kills:** subtracting after the union is written rather than before (both sole
/// words survive as empty terms); subtracting from the base's postings only (`flushsole` survives);
/// emitting an emptied term rather than dropping it (the key sets differ).
#[test]
fn a_deletion_in_any_layer_leaves_the_merged_index() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_text_fixture(&root, tmp.path());
    let engine = engine_over(tmp.path(), &root);
    let built = source_to_new_map(&root, "v00000");

    let flushed = ingest_and_flush(&engine, &root);
    let from_base = EntityId::new(built[&7]);
    let from_flush = flushed[1];

    engine
        .accept_change(from_base, ChangeOp::Delete)
        .expect("a deletion of a built item is accepted");
    engine
        .accept_change(from_flush, ChangeOp::Delete)
        .expect("a deletion of a flushed item is accepted");

    fold(&engine);

    let index = base_index(&root);
    assert!(
        !index.contains_key("builtsole"),
        "the deleted built item was the only carrier of its word and it survives: {:?}",
        index.keys().collect::<Vec<_>>()
    );
    assert!(
        !index.contains_key("flushsole"),
        "the deleted flushed item's word survives, so the merge subtracted from the base alone: \
         {:?}",
        index.keys().collect::<Vec<_>>()
    );
    let gone = [from_base.raw() as u32, from_flush.raw() as u32];
    for (word, carriers) in &index {
        for entity in gone {
            assert!(
                !carriers.contains(entity),
                "'{word}' still names a deleted entity"
            );
        }
    }
    let expected = expected_index(&built, &flushed, &gone);
    assert_eq!(
        index.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "the merged dictionary is not the survivors' vocabulary"
    );
    for (word, carriers) in &expected {
        assert_eq!(&index[word], carriers, "'{word}' names the wrong entities");
    }
}

/// **The fold consumes the layers it merged, and the prefix it publishes names none of them.**
///
/// The merge being right is half of it; the other half is bookkeeping, and getting it wrong fails
/// in two opposite ways. An extent left in the new manifest is counted twice — its entities are in
/// the merged base *and* in a layer beside it — and one whose files were dropped while the entry
/// stood is a prefix that refuses to open at all. Asserted against the published manifest and the
/// directory together, because either alone passes on one of the two.
#[test]
fn the_folded_prefix_lists_no_text_extent_and_holds_no_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_text_fixture(&root, tmp.path());
    let engine = engine_over(tmp.path(), &root);
    let built = source_to_new_map(&root, "v00000");

    let flushed = ingest_and_flush(&engine, &root);
    let extents = text_extents(&root);
    assert_eq!(extents.len(), 1);
    let old_prefix = current_prefix(&root);

    fold(&engine);

    let new_prefix = current_prefix(&root);
    assert_ne!(new_prefix, old_prefix, "the fold published a new prefix");
    assert!(
        text_extents(&root).is_empty(),
        "the fold merged every text layer into the base, so the new manifest names none"
    );
    // The consumed extent's files are not under the new prefix — nothing linked them forward, and
    // nothing names them.
    for rel in extents
        .iter()
        .flat_map(|e| [&e.dict, &e.postings, &e.presence])
    {
        assert!(
            !root.join(&new_prefix).join(rel).exists(),
            "a consumed text extent was hard-linked into the folded prefix: {rel}"
        );
    }
    // And every file the new manifest *does* name is there and digest-clean, which is what
    // `open_bundle` checks — including the two the text pass wrote.
    let bundle = open_bundle(&root).expect("the folded prefix opens and verifies");
    let named: Vec<&String> = bundle
        .manifest
        .files
        .keys()
        .filter(|rel| rel.contains("attrs/prose"))
        .collect();
    assert_eq!(
        named.len(),
        2,
        "the folded prefix should name exactly the merged dictionary and its postings: {named:?}"
    );
    // The corpus is still searchable through the artefacts the fold wrote.
    let index = base_index(&root);
    let expected = expected_index(&built, &flushed, &[]);
    assert_eq!(
        index.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
}

/// **A flush's text extent is digested, and the fold that carries one forward publishes.**
///
/// The two halves are one defect and were found by two reviewers independently. `write_text_extents`
/// wrote three files and named them in the side-manifest without ever inserting their digests —
/// unlike the record extent immediately beside it — and `publish_fold` discards outright when a
/// file it must carry forward has no digest in either live manifest. So the failure was not "a
/// missing integrity check": **a deployment with a searchable prose column and continuous ingest
/// would have folded, spent minutes to hours rewriting the corpus, discarded at the last step,
/// left an orphan prefix, and retired no deletion — for ever**, since the next trigger re-plans
/// into the same wall.
///
/// The three earlier cases in this file cannot reach it: they all flush *before* folding, so every
/// extent is consumed and none is carried. This one publishes a second extent after the plan's
/// snapshot, which is the flight case.
///
/// **Mutations this kills:** dropping any of the three digest insertions in `write_text_extents`
/// (the fold discards); dropping the text extents from `carried_rels` (the publication cannot link
/// the files and discards); dropping them from the new manifest's `text_extents` (the flushed
/// batch's words answer no `match` afterwards).
#[test]
fn a_text_extent_published_after_the_snapshot_is_carried_and_digested() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_text_fixture(&root, tmp.path());
    let engine = engine_over(tmp.path(), &root);
    let built = source_to_new_map(&root, "v00000");

    let first = ingest_and_flush(&engine, &root);

    // Every file the extent names is digested in the side-manifest that names it. Asserted on its
    // own, because it is what the fold depends on and the dependency is not visible from either
    // side.
    let bundle = open_bundle(&root).expect("the bundle opens");
    let files = &bundle.partitions["default"].manifest.files;
    for extent in text_extents(&root) {
        for rel in [&extent.dict, &extent.postings, &extent.presence] {
            assert!(
                files.contains_key(rel),
                "the flush named '{rel}' in the manifest and did not digest it; a fold carrying it \
                 forward discards, and nothing verifies its bytes"
            );
        }
    }

    // A second flush, whose extent the fold's plan cannot have seen. It is taken here rather than
    // by racing the fold thread — the plan snapshots the live manifest, so an extent published
    // after `request_fold` returns is indistinguishable from one published mid-flight, and racing
    // a background thread would make the case timing-dependent for nothing.
    let second = {
        let external = "flight-0".to_string();
        let row = UnallocatedRow {
            external_id: Some(external.as_bytes().to_vec()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"0".to_vec()],
            x: 20.0,
            y: 20.0,
            scalars: vec![WalScalar::Utf8("corpus flightword".to_string())],
            terms: engine.resolve_terms(&[b"0".to_vec()]),
            scoped: Vec::new(),
        };
        let id = engine
            .accept_ingest(vec![row], external, [0u8; 32])
            .expect("the flight ingest is accepted")[0];
        engine.request_flush();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while text_extents(&root).len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the second flush never published its text layer"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        id
    };

    fold(&engine);

    // Both flushes' words are in the merged base — the fold consumed both extents, which is what
    // the plan named. What this case is really about is that it *published at all*.
    let index = base_index(&root);
    assert!(
        index.contains_key("flightword"),
        "the second flush's words are missing from the folded index"
    );
    assert!(index["corpus"].contains(second.raw() as u32));
    let mut flushed = first;
    flushed.push(second);
    for entity in &flushed {
        assert!(
            index["corpus"].contains(entity.raw() as u32),
            "a flushed entity lost its words at the fold"
        );
    }
    assert_eq!(
        index["corpus"].cardinality(),
        N + FLUSHED + 1,
        "the merged posting is not every entity that carries the word"
    );
    assert!(
        text_extents(&root).is_empty(),
        "the fold consumed both extents"
    );
    let _ = built;
}

/// **A flush whose batch carries no value for a text column publishes no layer for it.**
///
/// An empty extent is not free. Nothing coalesces text layers — the entity-space coalesce skips any
/// column whose layers carry a dictionary — so every one of them survives until the next fold, and
/// every `match` pays a dictionary resolve and a posting read per token against each. A layer that
/// can only ever answer the empty set is a permanent per-query cost buying nothing.
///
/// The batch here carries a *null*, which is absence. An entity whose prose is the empty string
/// carries a value and no terms, and must still get a layer — that is why the flush tests presence
/// rather than the term map, and why this case ingests both.
#[test]
fn a_flush_with_no_text_value_publishes_no_text_layer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_text_fixture(&root, tmp.path());
    let engine = engine_over(tmp.path(), &root);

    let ingest = |engine: &Engine, name: &str, value: WalScalar| {
        let row = UnallocatedRow {
            external_id: Some(name.as_bytes().to_vec()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"0".to_vec()],
            x: 30.0,
            y: 30.0,
            scalars: vec![value],
            terms: engine.resolve_terms(&[b"0".to_vec()]),
            scoped: Vec::new(),
        };
        engine
            .accept_ingest(vec![row], name.to_string(), [0u8; 32])
            .expect("the ingest is accepted")[0]
    };

    ingest(&engine, "null-prose", WalScalar::Null);
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never completed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        text_extents(&root).is_empty(),
        "a batch with no prose published a text layer that can only answer the empty set"
    );

    // The empty string is a value a corpus may hold, and it does get a layer: it carries a presence
    // bit and no terms, so a later `exists` predicate must be able to find it.
    let empty = ingest(&engine, "empty-prose", WalScalar::Utf8(String::new()));
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while text_extents(&root).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the empty-string batch published no text layer"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let extents = text_extents(&root);
    assert_eq!(extents.len(), 1, "exactly the empty-string batch's layer");
    let present = croaring::Bitmap::deserialize::<croaring::Portable>(
        &std::fs::read(root.join(current_prefix(&root)).join(&extents[0].presence))
            .expect("the presence bitmap reads"),
    );
    assert!(
        present.contains(empty.raw() as u32),
        "an entity whose prose is the empty string carries a value and must be in the layer's \
         presence, where no posting can name it"
    );
}
