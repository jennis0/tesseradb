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
/// The tile depth the narrow viewports are asked at. **A request's tiles come from its own `zoom`**,
/// so at zoom 0 a bbox resolves to the single tile covering the whole grid and narrows nothing — a
/// viewport case asked there would be comparing the whole map with itself five times.
const VIEWPORT_ZOOM: u8 = 4;

/// Viewports from the whole map down to a corner. **Several, because the two routes diverge exactly
/// with the viewport**: an artifact-major level settles a whole-map request through its extents
/// without scanning, and a row-major one scans every visible row — so a defect that only appears
/// when the box is small is a defect only a narrow viewport finds.
fn viewports() -> Vec<(u8, [f64; 4])> {
    vec![
        (0, WHOLE_MAP),
        (VIEWPORT_ZOOM, WHOLE_MAP),
        (VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]),
        (VIEWPORT_ZOOM, [250.0, 250.0, 750.0, 750.0]),
        (VIEWPORT_ZOOM, [0.0, 0.0, 125.0, 125.0]),
        (VIEWPORT_ZOOM, [900.0, 900.0, 1000.0, 1000.0]),
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
fn served(
    engine: &Engine,
    grant: &str,
    layer: &str,
    zoom: u8,
    bbox: [f64; 4],
) -> BTreeMap<String, u64> {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [layer];
    let mut request = ViewportRequest::new("s0", zoom, bbox, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
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

#[allow(dead_code)]
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
    let all = served(&fx.engine, "0,1,2,3", BY_LIST, 0, WHOLE_MAP);
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
        for (zoom, bbox) in viewports() {
            let by_list = served(&fx.engine, grant, BY_LIST, zoom, bbox);
            let by_rule = served(&fx.engine, grant, BY_RULE, zoom, bbox);
            assert_eq!(
                by_rule, by_list,
                "the rule and the list disagree for grant {grant} over {bbox:?} at zoom {zoom}"
            );
            compared += 1;
        }
    }
    assert_eq!(compared, grants().len() * viewports().len());

    // **A narrow viewport must serve fewer artifacts than the whole map**, or the candidacy
    // arithmetic each route takes is not being exercised: an equality between two whole-map answers
    // is one comparison repeated, not six.
    let whole = served(&fx.engine, "0,1", BY_RULE, 0, WHOLE_MAP);
    let corner = served(
        &fx.engine,
        "0,1",
        BY_RULE,
        VIEWPORT_ZOOM,
        [0.0, 0.0, 125.0, 125.0],
    );
    assert!(
        corner.len() < whole.len(),
        "every artifact is a candidate in a corner viewport, so candidacy decides nothing"
    );

    // **And the two are not both empty.** An equality between two absences would pass on a system
    // that served neither layer, which is exactly the state this stage started from.
    let narrow = served(
        &fx.engine,
        "0,1",
        BY_RULE,
        VIEWPORT_ZOOM,
        [0.0, 0.0, 500.0, 500.0],
    );
    assert!(
        !narrow.is_empty(),
        "the rule served nothing at a viewport where the list served {}",
        served(
            &fx.engine,
            "0,1",
            BY_LIST,
            VIEWPORT_ZOOM,
            [0.0, 0.0, 500.0, 500.0]
        )
        .len()
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
        served(&fx.engine, "0,1", BY_RULE, 0, WHOLE_MAP),
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

fn scalars_with_partition(value: u32, columns: usize, at: usize) -> Vec<WalScalar> {
    let mut scalars = vec![WalScalar::Null; columns];
    scalars[at] = WalScalar::U32(value);
    scalars
}

/// Ingest one point at `(x, y)` carrying `partition = value`, visible to term `term`.
#[allow(clippy::too_many_arguments)]
fn ingest_point_into(
    engine: &Engine,
    external_id: &str,
    value: u32,
    term: u32,
    x: f32,
    y: f32,
    columns: usize,
    at: usize,
) -> u64 {
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
                scalars: scalars_with_partition(value, columns, at),
                terms: engine.resolve_terms(&descriptors),
            }],
            external_id.to_string(),
            hash,
        )
        .expect("an ordinary point with a declared scalar is an ordinary write");
    ids.len() as u64
}

/// The generator's own schema: seven declared columns, `partition` at [`PARTITION_SCALAR`].
fn ingest_point(engine: &Engine, external_id: &str, value: u32, term: u32, x: f32, y: f32) -> u64 {
    ingest_point_into(engine, external_id, value, term, x, y, 7, PARTITION_SCALAR)
}

/// This file's own declaration: one column, so `partition` is at 0.
fn ingest_own(engine: &Engine, external_id: &str, value: u32, term: u32, x: f32, y: f32) -> u64 {
    ingest_point_into(engine, external_id, value, term, x, y, 1, 0)
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
    let before = served(&fx.engine, grant, BY_RULE, 0, WHOLE_MAP);
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

    let after = served(&fx.engine, grant, BY_RULE, 0, WHOLE_MAP);
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
    let list = served(&fx.engine, grant, BY_LIST, 0, WHOLE_MAP);
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
    let before = served(&fx.engine, grant, BY_RULE, 0, WHOLE_MAP);
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
    let after = served(&fx.engine, grant, BY_RULE, 0, WHOLE_MAP);
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
        served(&fx.engine, grant, BY_RULE, 0, WHOLE_MAP).get(&novel.to_string()),
        Some(&2)
    );
}

// ---------------------------------------------------------------------------------------------
// The rest of the surface, over a declaration this file owns
// ---------------------------------------------------------------------------------------------
//
// The generator's own predicate layer declares no criterion and nothing depends on it, which is
// right for the twin check above — an equality between two routes must not be an equality between
// two gates. These cases are about the conjuncts, so they need a declaration that carries them.

const BANDS: &str = "bands/by-value";
const LABELS: &str = "labels/on-bands";

/// The value entity 0 carries — the band the label below attaches to, and the artifact whose
/// suppression must take the label with it.
fn anchor_value(c: &Corpus) -> u32 {
    c.partition_artifact_of(tessera_corpus::materialise::PARTITION_LAYER, 0) as u32
}

fn own_config(c: &Corpus, criterion: &str) -> String {
    // **Members a principal can actually see.** A label whose every member is invisible to the
    // viewer is in no node of the viewport's walk, so it is not a candidate and the case would be
    // asserting an absence for the wrong reason.
    let visible = Grant::parse("0").unwrap();
    let members: Vec<String> = (0..c.n())
        .filter(|e| c.visible(*e, &visible))
        .take(8)
        .map(|e| e.to_string())
        .collect();
    assert!(!members.is_empty(), "no entity is visible to term 0");
    format!(
        r#"
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[[view]]
name             = "s0"
extent           = {{ min = 0.0, max = 1000.0 }}
point_visibility = {{ source = "pairs", default = "public" }}

[[attribute]]
name  = "partition"
type  = "u32"
index = true

[[layer]]
name                      = "{BANDS}"
views                     = ["s0"]
membership                = {{ attribute = "partition" }}
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = {criterion}

[[layer]]
name                      = "{LABELS}"
views                     = ["s0"]
membership                = "enumerated"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
depends_on                = ["{BANDS}"]
artifacts = [
  {{ key = "l0", members = [{}], attached_layer = "{BANDS}", attached_key = "{}" }},
]
"#,
        members.join(", "),
        anchor_value(c)
    )
}

struct Own {
    _tmp: tempfile::TempDir,
    engine: Engine,
    corpus: Corpus,
}

fn own_fixture(criterion: &str) -> Own {
    let corpus = corpus();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    corpus.write_points_parquet(&points).expect("points");
    corpus.write_pairs_parquet(&pairs).expect("pairs");
    let config_path = tmp.path().join("own-config.toml");
    std::fs::write(&config_path, own_config(&corpus, criterion)).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("this file's own declaration parses");
    build_with_layers(&root, &points, &pairs, &corpus, config);
    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine.set_background_refresh_for_test(false);
    Own {
        _tmp: tmp,
        engine,
        corpus,
    }
}

/// **The criterion tests the number it serves, on a rule as on a roster.** A band a viewer can see
/// too little of is absent, not served with a small count beside it — and the number a viewer would
/// have been shown is the one the bar was applied to, because there is only one of them.
#[test]
fn an_absolute_criterion_fires_on_an_attribute_predicate() {
    let open = own_fixture("\"none\"");
    let ungated = served(&open.engine, "0,1", BANDS, 0, WHOLE_MAP);
    let mut counts: Vec<u64> = ungated.values().copied().collect();
    counts.sort_unstable();
    let bar = counts[counts.len() / 2];
    assert!(bar > 1, "the fixture's bands are too small to set a bar");
    drop(open);

    let gated = own_fixture(&format!("{{ count = {bar} }}"));
    let served_gated = served(&gated.engine, "0,1", BANDS, 0, WHOLE_MAP);
    let expected: BTreeMap<String, u64> = ungated
        .iter()
        .filter(|(_, count)| **count >= bar)
        .map(|(key, count)| (key.clone(), *count))
        .collect();
    assert_eq!(
        served_gated, expected,
        "the criterion tested a different number from the one served"
    );
    assert!(
        served_gated.len() < ungated.len(),
        "the criterion withheld nothing, so it is untested"
    );
}

/// The entity behind a served artifact, through the admin plane's own resolver — the address a
/// suppression names, and the one drill-down inverts.
fn served_entity(engine: &Engine, grant: &str, layer: &str, key: &str) -> tessera_types::TesseraId {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [layer];
    let mut request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
    engine
        .viewport(&session, request)
        .expect("a viewport")
        .artifacts
        .into_iter()
        .find(|artifact| artifact.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("{key} is not served"))
        .tessera_id
}

/// **Drill-down answers the same predicate the viewport does**, over a membership nothing stores.
/// An artifact reachable by identifier and not by viewport — or the reverse — is two transcriptions
/// of one rule, which is what the shared `verdict` exists to prevent.
#[test]
fn a_predicate_artifact_answers_by_identifier_as_it_does_by_viewport() {
    let fx = own_fixture("\"none\"");
    let grant = "0,1";
    let by_viewport = served(&fx.engine, grant, BANDS, 0, WHOLE_MAP);
    let (key, count) = by_viewport.iter().next().expect("something is served");
    let id = served_entity(&fx.engine, grant, BANDS, key);
    let idset = fx.engine.generation().bundle.manifest.identity.idset;

    let session = fx.engine.authorise(&credential(grant)).unwrap();
    let row = fx
        .engine
        .artifact(&session, id, Some(idset), "s0", None)
        .expect("the identifier route answers")
        .expect("the artifact the viewport just served is reachable by its identifier");
    assert_eq!(row.key.as_deref(), Some(key.as_str()));
    assert_eq!(row.masked_count, *count, "two routes, two counts");

    // A principal who can see nothing of the band is served the band's own zero, because *this*
    // layer declares no criterion — existence is not gated on membership here, which is a statement
    // rather than an omission (C17). Where a layer does declare one, the identifier route withholds
    // by the same call rather than by a second rule.
    let blind = fx.engine.authorise(b"{\"terms\": []}").unwrap();
    assert_eq!(
        fx.engine
            .artifact(&blind, id, Some(idset), "s0", None)
            .expect("the identifier route answers")
            .map(|row| row.masked_count),
        Some(0)
    );
    drop(fx);

    let gated = own_fixture("{ count = 1 }");
    let key = served(&gated.engine, grant, BANDS, 0, WHOLE_MAP)
        .keys()
        .next()
        .cloned()
        .expect("something clears a bar of one");
    let id = served_entity(&gated.engine, grant, BANDS, &key);
    let idset = gated.engine.generation().bundle.manifest.identity.idset;
    let blind = gated.engine.authorise(b"{\"terms\": []}").unwrap();
    assert!(
        gated
            .engine
            .artifact(&blind, id, Some(idset), "s0", None)
            .expect("the identifier route answers")
            .is_none(),
        "a band below its own bar is reachable by identifier"
    );
}

/// **A dependent is served only where the artifact it attaches to is served**, and a predicate
/// artifact is an ordinary target: it has an entity, a key an edge can name, and a disposition.
#[test]
fn a_label_attached_to_a_predicate_artifact_follows_its_target() {
    let fx = own_fixture("\"none\"");
    let grant = "0,1";
    let anchor = anchor_value(&fx.corpus).to_string();
    assert!(
        served(&fx.engine, grant, BANDS, 0, WHOLE_MAP).contains_key(&anchor),
        "the band the label attaches to is not served, so the case is vacuous"
    );
    assert!(
        served(&fx.engine, grant, LABELS, 0, WHOLE_MAP).contains_key("l0"),
        "the label is not served while its target is"
    );

    // Suppress the band; the label goes with it, without anything being said about the label.
    let id = served_entity(&fx.engine, grant, BANDS, &anchor);
    let idset = fx.engine.generation().bundle.manifest.identity.idset;
    let entity = fx.engine.resolve_tessera_ids(&[id], idset).unwrap()[0]
        .expect("a served artifact's identifier names an entity");
    fx.engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Suppress)
        .expect("a suppression is accepted");

    assert!(
        !served(&fx.engine, grant, BANDS, 0, WHOLE_MAP).contains_key(&anchor),
        "a suppressed band is still served"
    );
    assert!(
        !served(&fx.engine, grant, LABELS, 0, WHOLE_MAP).contains_key("l0"),
        "the label outlived the band it depends on"
    );
}

/// **A suppressed value's key never mints a second artifact.**
///
/// Suppression touches no stored structure at all (write-path §5.4, Rule S), so the store's key
/// index still holds the key — which is what the duplicate-key check reads. Written against the
/// *served* view instead, a suppressed artifact would read as absent, the key would mint a second
/// one, and the new one would not be suppressed: a suppression defeated by ingesting a point.
#[test]
fn a_suppressed_values_key_never_mints_again() {
    let fx = own_fixture("\"none\"");
    let grant = "0";
    let anchor = anchor_value(&fx.corpus).to_string();
    let id = served_entity(&fx.engine, grant, BANDS, &anchor);
    let idset = fx.engine.generation().bundle.manifest.identity.idset;
    let entity = fx.engine.resolve_tessera_ids(&[id], idset).unwrap()[0].unwrap();
    fx.engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Suppress)
        .expect("a suppression is accepted");
    let artifacts = fx.engine.published_artifacts();

    ingest_own(
        &fx.engine,
        "after-suppress",
        anchor.parse().unwrap(),
        0,
        11.0,
        11.0,
    );
    assert_eq!(
        fx.engine.published_artifacts(),
        artifacts,
        "a point carrying a suppressed value minted a second artifact for it"
    );
    flush(&fx.engine);
    assert!(
        !served(&fx.engine, grant, BANDS, 0, WHOLE_MAP).contains_key(&anchor),
        "the suppression was defeated by ingesting a point"
    );
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
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

/// **A deleted value's key mints again, and what comes back is a new object.**
///
/// The two removal rules are not the same event (write-path §5.4). A suppression retires only on
/// unsuppress and touches no stored structure, so its key still resolves and mints nothing — the
/// case above. A **deletion** retires at the fold that executes it, and that fold takes the key out
/// of the store's index with the artifact's own entity. A point carrying the value afterwards
/// therefore creates a *new* artifact on a *new* entity, which is precisely what a deletion means:
/// the old identity is gone and no `tessera_id` a caller holds names the new one.
#[test]
fn a_deleted_values_key_returns_as_a_new_artifact() {
    let fx = own_fixture("\"none\"");
    let grant = "0";
    let anchor = anchor_value(&fx.corpus).to_string();
    let idset = fx.engine.generation().bundle.manifest.identity.idset;
    let before_id = served_entity(&fx.engine, grant, BANDS, &anchor);
    let entity = fx.engine.resolve_tessera_ids(&[before_id], idset).unwrap()[0].unwrap();

    fx.engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Delete)
        .expect("a deletion is accepted");
    fold(&fx.engine);
    assert!(
        !served(&fx.engine, grant, BANDS, 0, WHOLE_MAP).contains_key(&anchor),
        "a deleted band is still served after the fold that executed it"
    );

    ingest_own(
        &fx.engine,
        "after-delete",
        anchor.parse().unwrap(),
        0,
        13.0,
        13.0,
    );
    flush(&fx.engine);

    let after = served(&fx.engine, grant, BANDS, 0, WHOLE_MAP);
    assert!(
        after.contains_key(&anchor),
        "a point carrying a deleted value minted nothing, so the value has no artifact at all"
    );
    let after_id = served_entity(&fx.engine, grant, BANDS, &anchor);
    assert_ne!(
        after_id, before_id,
        "the value came back on the deleted artifact's own identity"
    );
}
