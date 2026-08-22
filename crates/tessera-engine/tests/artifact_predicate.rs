//! **The same layer, built by rule and built by list, serves identically** — Stage 6's own check,
//! over the generator's partition arm.
//!
//! `generator/partition-enumerated` and `generator/partition-attribute` are one relation written
//! two ways: the first stores a member list per artifact, the second declares
//! `membership = { attribute = "partition" }` and stores nothing at all. Every point carries
//! exactly one partition value, so the two are the same partition of the corpus — and a viewer must
//! not be able to tell which is which. That is what this file asserts, artifact for artifact, over
//! several principals and several viewports.
//!
//! **The comparison is between two *served* answers, not between an answer and an expectation.**
//! An expectation this file wrote down would be a third transcription of the relation and would
//! pass on a system where both routes were wrong the same way. What makes the check bite is that
//! the two routes share nothing below the declaration: one projects stored memberships into row
//! space and walks a tile index, the other permutes an entity-addressed value column into a label
//! per row and scans `viewport ∩ M_auth`. A defect in either shows up as a disagreement.
//!
//! **Engine-level rather than over HTTP**, and the reason is what the case is about: the two layers
//! must agree *inside the trust boundary*, at every viewport a principal can ask for, and the
//! viewport surface is where both are computed. `membership_column.rs` drives the equivalent
//! build/ingest case over the wire because its subject is the wire — what a client can tell about
//! the route a point took. Here the wire would add a serialisation and remove nothing.
//!
//! The freshness half is here too: a point ingested with an existing value counts against its
//! value's artifact on the **next request**, with nothing rebuilt.

mod common;

use std::collections::BTreeMap;

use common::*;
use tessera_corpus::{Corpus, Grant};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalScalar;

/// Small enough for the ordinary test pass, and large enough that the partition arm has more than a
/// handful of values and the masked counts are numbers rather than zeros and ones.
const N: u64 = 3_000;
const SEED: u64 = 0x5EED;

/// The generator's two spellings of one relation (`tessera_corpus::materialise`'s declaration).
const BY_LIST: &str = "generator/partition-enumerated";
const BY_RULE: &str = "generator/partition-attribute";

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// Viewports from the whole map down to a corner. **Several, because the two routes diverge exactly
/// with the viewport**: an artifact-major level settles a whole-map request through its extents
/// without scanning, and a row-major one scans every visible row — so a defect that only appears
/// when the box is small is a defect only a narrow viewport finds.
fn viewports() -> Vec<[f64; 4]> {
    vec![
        WHOLE_MAP,
        [0.0, 0.0, 500.0, 500.0],
        [250.0, 250.0, 750.0, 750.0],
        [0.0, 0.0, 125.0, 125.0],
        [900.0, 900.0, 1000.0, 1000.0],
    ]
}

/// The principals this is checked for. **A viewer seeing everything is not enough**: the whole
/// point of the surface is that a count is the size of an intersection with the viewer's own
/// visible set, so a grant that covers the corpus would compare two unmasked answers and never
/// exercise the masking at all.
fn grants() -> Vec<&'static str> {
    vec!["0", "0,1", "1,2,3", "2", "5,6,7,8"]
}

fn corpus() -> Corpus {
    Corpus::new(SEED, N, extent()).expect("the generator accepts the fixture's extent")
}

fn credential(grant: &str) -> Vec<u8> {
    let terms: Vec<String> = Grant::parse(grant)
        .expect("the grant is inside the generator's term space")
        .terms()
        .iter()
        .map(|t| format!("\"{}\"", t.raw()))
        .collect();
    format!("{{\"terms\": [{}]}}", terms.join(", ")).into_bytes()
}

/// What one layer serves one principal at one viewport: every artifact's key against its masked
/// count, plus whether it carried a parent — the whole of what a client can read off an artifact
/// row that is not an identifier.
fn served(engine: &Engine, grant: &str, layer: &str, bbox: [f64; 4]) -> BTreeMap<String, u64> {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [layer];
    let mut request = ViewportRequest::new("s0", 0, bbox, N as usize);
    request.layers = Some(&names);
    engine
        .viewport(&session, request)
        .expect("a viewport over the fixture")
        .artifacts
        .into_iter()
        .map(|artifact| {
            assert_eq!(
                artifact.layer, layer,
                "a response carried a layer the request did not name"
            );
            (
                artifact.key.expect("both layers publish keyed artifacts"),
                artifact.masked_count,
            )
        })
        .collect()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    engine: Engine,
    corpus: Corpus,
}

fn fixture() -> Fixture {
    let corpus = corpus();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    corpus.write_points_parquet(&points).expect("points");
    corpus.write_pairs_parquet(&pairs).expect("pairs");
    build_corpus_fixture_with_layers(&root, &points, &pairs, &corpus);

    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine.set_background_refresh_for_test(false);
    Fixture {
        _tmp: tmp,
        engine,
        corpus,
    }
}

/// **Stage 6's check.** One layer by rule and one by list, over the same relation: identical masked
/// counts and identical served sets, for every principal and every viewport.
#[test]
fn the_same_layer_by_rule_and_by_list_serves_identically() {
    let fx = fixture();

    // The fixture has to have something in it, or the equality below is vacuous.
    let all = served(&fx.engine, "0,1,2,3", BY_LIST, WHOLE_MAP);
    assert!(
        all.len() > 4,
        "the partition arm served {} artifacts, which is too few for this to mean anything",
        all.len()
    );
    assert!(
        all.values().any(|count| *count > 1),
        "every served count is 0 or 1, so the masked arithmetic is untested"
    );

    let mut compared = 0usize;
    for grant in grants() {
        for bbox in viewports() {
            let by_list = served(&fx.engine, grant, BY_LIST, bbox);
            let by_rule = served(&fx.engine, grant, BY_RULE, bbox);
            assert_eq!(
                by_rule, by_list,
                "the rule and the list disagree for grant {grant} over {bbox:?}"
            );
            compared += 1;
        }
    }
    assert_eq!(compared, grants().len() * viewports().len());

    // **And the two are not both empty.** An equality between two absences would pass on a system
    // that served neither layer, which is exactly the state this stage started from.
    let narrow = served(&fx.engine, "0,1", BY_RULE, [0.0, 0.0, 500.0, 500.0]);
    assert!(
        !narrow.is_empty(),
        "the rule served nothing at a viewport where the list served {}",
        served(&fx.engine, "0,1", BY_LIST, [0.0, 0.0, 500.0, 500.0]).len()
    );
}

/// **The engine's answer is the generator's answer**, not merely the same as its twin's.
///
/// The check above compares two routes; this compares one of them to a number computed outside the
/// engine entirely. Together they say the pair is right rather than consistently wrong.
#[test]
fn the_rule_serves_the_masked_count_the_generator_computes() {
    let fx = fixture();
    let grant = Grant::parse("0,1").unwrap();

    // The generator's own partition relation, masked by hand: every entity's value, counted where
    // the principal can see the entity.
    let mut expected: BTreeMap<String, u64> = BTreeMap::new();
    for e in 0..fx.corpus.n() {
        if !fx.corpus.visible(e, &grant) {
            continue;
        }
        let value = fx
            .corpus
            .partition_artifact_of(tessera_corpus::materialise::PARTITION_LAYER, e);
        *expected.entry(value.to_string()).or_default() += 1;
    }
    // An artifact this principal can see nothing of is absent from a response rather than served
    // with a zero, and the generator's map has no zeros in it by construction.
    expected.retain(|_, count| *count > 0);

    assert_eq!(
        served(&fx.engine, "0,1", BY_RULE, WHOLE_MAP),
        expected,
        "the rule's masked counts are not the ones the generator computes"
    );
}

// ---------------------------------------------------------------------------------------------
// Freshness: the membership is the column, so a point that arrives is a member
// ---------------------------------------------------------------------------------------------

/// The generator's schema, in declared order — `fx_key`, `weight`, `seen_at`, `bay`, `tag`,
/// `blurb`, `partition` (`tessera_corpus::materialise`'s `CONFIG_TOML`). The ingest wire builds a
/// row's vector in that order, so a value lands in the column its position names.
const PARTITION_SCALAR: usize = 6;

fn scalars_with_partition(value: u32) -> Vec<WalScalar> {
    let mut scalars = vec![WalScalar::Null; 7];
    scalars[PARTITION_SCALAR] = WalScalar::U32(value);
    scalars
}

/// Ingest one point at `(x, y)` carrying `partition = value`, visible to term `term`.
fn ingest_point(engine: &Engine, external_id: &str, value: u32, term: u32, x: f32, y: f32) -> u64 {
    let descriptors = vec![term.to_string().into_bytes()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(external_id.as_bytes()) {
        *slot = *byte;
    }
    let ids = engine
        .accept_ingest(
            vec![UnallocatedRow {
                external_id: Some(external_id.as_bytes().to_vec()),
                view: "s0".to_string(),
                descriptors: descriptors.clone(),
                x,
                y,
                scalars: scalars_with_partition(value),
                terms: engine.resolve_terms(&descriptors),
            }],
            external_id.to_string(),
            hash,
        )
        .expect("an ordinary point with a declared scalar is an ordinary write");
    ids.len() as u64
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

/// **A point ingested with value *v* counts against *v*'s artifact on the next request, with
/// nothing rebuilt.**
///
/// "Nothing rebuilt" is stated as three things that must not have happened: no fold, no publication
/// into the layer, and no growth — the level's own write counter is unchanged, so no membership was
/// written anywhere. What *did* move is the geometry, which is the point: the membership is the
/// column, so a row that appears is a member the moment it has a row.
///
/// ⊘ **The moment is the flush, not the acknowledgement**, and that is the same bounded lag every
/// other entity-space derivation has (`filter-index.md` §5): a buffered point has no row and no
/// extent, so it contributes to no row-addressed structure until its flush. It only ever
/// *withholds*, never over-counts, which is the fail-closed direction.
#[test]
fn a_point_ingested_with_a_value_counts_on_the_next_request() {
    let fx = fixture();
    let grant = "0";
    let before = served(&fx.engine, grant, BY_RULE, WHOLE_MAP);
    let value = *before
        .keys()
        .next()
        .expect("the rule serves something to start from")
        .parse::<u32>()
        .as_ref()
        .expect("a plain integer column's keys are decimal");
    let was = before[&value.to_string()];
    let versions = fx.engine.write_executor_stats();

    ingest_point(&fx.engine, "fresh-1", value, 0, 5.0, 5.0);
    flush(&fx.engine);

    let after = served(&fx.engine, grant, BY_RULE, WHOLE_MAP);
    assert_eq!(
        after[&value.to_string()],
        was + 1,
        "the ingested point did not reach its value's artifact"
    );
    assert_eq!(
        fx.engine.write_executor_stats().folds,
        versions.folds,
        "the count moved because of a fold rather than because of the column"
    );
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>(),
        "an existing value minted a second artifact, or moved one"
    );

    // And the twin agrees about the *old* rows: an enumerated layer's membership is stored, so the
    // new point is in none of its artifacts. The two differ here by exactly one document, which is
    // the difference between a stored answer and a live rule — and is why a predicate layer is
    // never stale while an enumerated one is between the write and the refresh.
    let list = served(&fx.engine, grant, BY_LIST, WHOLE_MAP);
    assert_eq!(
        list[&value.to_string()],
        was,
        "the stored membership grew without anyone writing to it"
    );
}

/// **A value nothing has ever carried mints its artifact at the window's close.**
///
/// The rule produces the identities, so a value that appears is an artifact that appears — and it
/// is created by the write that carries it rather than by a later publication, which is what makes
/// it addressable (a suppression has somewhere to land) before the next request reads it.
#[test]
fn a_new_value_mints_its_artifact_at_the_windows_close() {
    let fx = fixture();
    let grant = "0";
    let before = served(&fx.engine, grant, BY_RULE, WHOLE_MAP);
    // A value far outside the generator's own partition space, so nothing holds it.
    let novel = 900_001u32;
    assert!(!before.contains_key(&novel.to_string()));
    let artifacts_before = fx.engine.published_artifacts();

    ingest_point(&fx.engine, "novel-1", novel, 0, 7.0, 7.0);
    assert_eq!(
        fx.engine.published_artifacts(),
        artifacts_before + 1,
        "the novel value minted no artifact at the window's close"
    );

    flush(&fx.engine);
    let after = served(&fx.engine, grant, BY_RULE, WHOLE_MAP);
    assert_eq!(
        after.get(&novel.to_string()),
        Some(&1),
        "the minted artifact does not carry the point that created it"
    );

    // **A second point under the same value mints nothing**, which is the duplicate-key rule the
    // build and the ingest share: a key a live artifact holds is never minted again.
    ingest_point(&fx.engine, "novel-2", novel, 0, 8.0, 8.0);
    assert_eq!(
        fx.engine.published_artifacts(),
        artifacts_before + 1,
        "the same value minted a second artifact"
    );
    flush(&fx.engine);
    assert_eq!(
        served(&fx.engine, grant, BY_RULE, WHOLE_MAP).get(&novel.to_string()),
        Some(&2)
    );
}
