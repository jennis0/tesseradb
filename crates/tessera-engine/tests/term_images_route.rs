//! **Every route to a session's row projection produces the same projection**, and the same
//! viewport response over it.
//!
//! A session's row projection is built by the walk over its whole fragment, by the row range where
//! its grant covers the entity domain, by the split route, which unions the bundle's images of the
//! terms it holds and then walks the residual and the extents, or by the complement route, a walk
//! over the entities the grant does not hold subtracted from the base's row range
//! (`tessera_engine::compose::RowProjection::new`). The chooser picks one from the principal's own
//! grant before any of them runs. What that buys is first-viewport time; what it must never cost
//! is a row.
//!
//! **The claim is an I2 claim, not a performance one.** An image is a term's base posting
//! projected at build or at fold, so a split route that unioned an image of a term the session
//! does not hold, or that walked a residual reaching outside the fragment, would serve rows the
//! principal was never granted; one whose residual missed an entity would serve fewer. The
//! complement route reaches the same claim from the other side: it reads the slots of entities the
//! principal does not hold, and a subtraction that took one row too few would serve a row the
//! grant does not carry. Neither is visible in a response read on its own, which is why every case
//! here compares the routes against each other and against `RowSpace::project`, the crossing the
//! whole system already rests on.
//!
//! **The routes are compared under forcing, not as chosen.** The chooser is a function of the
//! grant, so it picks the same route for the same principal every time, and a suite that compared
//! only chosen routes would compare one route with itself for every principal in it.
//! `Engine::force_projection_route_for_test` fixes the route, and
//! `Engine::projection_builds_by_route` is how each case establishes that the route it asked for
//! is the route that ran — a forced split with nothing to union falls back to the walk, and
//! without the gauge that fallback reads as a passing test.
//!
//! **The corpus is built so that both halves of the split do work.** Its terms are checked after
//! the build rather than assumed: `the_fixtures_terms_are_classified_as_every_other_case_assumes`
//! reads the image table and pins which terms have an image and which do not, because the keep
//! rule is about rows per Roaring container and no reasoning about the source lists settles it.

mod common;

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use croaring::Portable;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig, ProjectionRoute, ViewportRequest};
use tessera_lifecycle::{ChangeOp, UnallocatedRow};
use tessera_types::EntityId;

/// The one view the fixture builds. A second, created while the engine runs, appears in
/// [`a_view_created_while_running_has_no_images_and_is_served_by_the_walk`].
const VIEW: &str = "s0";

/// Entities in the fixture. Above one Roaring container of row space (65 536), so an image's
/// container count is a number the keep rule can actually turn on: at one container every term of
/// more than thirty rows would be kept and the residual half of the split would never be
/// exercised by anything but the terms too small to project at all.
const ENTITIES: u64 = 200_000;

// The fixture's access relation, by the label a credential names each term with. A label is the
// decimal spelling of the `term_id` column the pairs file carries.
/// Half the corpus. Kept.
const HALF: u32 = 11;
/// Every entity but the two hundred `THOUSANDTH` names. Kept, and the term that makes a grant
/// narrow enough outside for the complement to be the cheapest route.
const ALMOST_ALL: u32 = 12;
/// Almost all of it. Kept, and the broadest principal's term.
const MOST: u32 = 13;
/// A tenth. Kept.
const TENTH: u32 = 14;
/// A hundredth. Kept.
const HUNDREDTH: u32 = 15;
/// A thousandth, two hundred entities. Kept, and the narrowest principal's only kept term.
const THOUSANDTH: u32 = 16;
/// Eighty entities spread across the whole extent. **Projected and not kept** — its rows fall in
/// every container of row space, so it is under the keep rule's thirty rows per container. It is
/// the term that puts a term of real size into the residual walk rather than only the tiny ones.
const SCATTERED: u32 = 20;
/// The first of the small terms. Term `SMALL_BASE + n` carries `1 + n % 30` entities, so none of
/// them is projected at all and every one of them is residual.
const SMALL_BASE: u32 = 100;
/// How many small terms the fixture carries.
const SMALL_TERMS: u32 = 200;
/// The distance between two small terms' entity blocks.
const SMALL_STRIDE: u64 = 607;

/// Which terms entity `i` carries.
fn terms_of(i: u64) -> Vec<u32> {
    let mut terms = Vec::new();
    if i.is_multiple_of(2) {
        terms.push(HALF);
    }
    if i % 1000 != 5 {
        terms.push(ALMOST_ALL);
    }
    if !i.is_multiple_of(20) {
        terms.push(MOST);
    }
    if i % 10 == 3 {
        terms.push(TENTH);
    }
    if i % 100 == 7 {
        terms.push(HUNDREDTH);
    }
    if i % 1000 == 5 {
        terms.push(THOUSANDTH);
    }
    // **2 501 and not 2 500.** The points below place entity `i` at `(37i mod 1000, 53i mod
    // 1000)`, so a stride of 2 500 steps by `(500, 500)` and lands every member of the term on one
    // of two positions — two containers, and the term is kept. A stride coprime with the modulus
    // walks the whole extent, which is what "scattered" has to mean here.
    if i % 2501 == 9 {
        terms.push(SCATTERED);
    }
    let n = i / SMALL_STRIDE;
    if n < u64::from(SMALL_TERMS) && i - n * SMALL_STRIDE < 1 + n % 30 {
        terms.push(SMALL_BASE + n as u32);
    }
    terms.sort_unstable();
    terms
}

/// A label as a credential and an ingest batch spell it.
fn label(term: u32) -> Vec<u8> {
    term.to_string().into_bytes()
}

/// The credential naming `terms`.
fn credential(terms: &[u32]) -> Vec<u8> {
    let named: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
    format!("{{\"terms\": [{}]}}", named.join(", ")).into_bytes()
}

/// One principal of the suite, at a stated share of the corpus.
///
/// Every one of them holds **both** a term with an image and terms without one, so the split route
/// unions something and walks something for each. The narrowest holds two hundred entities and the
/// broadest ninety-five per cent, which is the span the chooser's own constants were modelled over.
struct Principal {
    name: &'static str,
    credential: Vec<u8>,
}

fn principals() -> Vec<Principal> {
    let small = |from: u32, to: u32| -> Vec<u32> { (from..to).collect() };
    let with = |head: &[u32], tail: Vec<u32>| -> Vec<u8> {
        let mut terms = head.to_vec();
        terms.extend(tail);
        credential(&terms)
    };
    vec![
        Principal {
            name: "0.1%",
            credential: with(&[THOUSANDTH], small(SMALL_BASE, SMALL_BASE + 10)),
        },
        Principal {
            name: "1%",
            credential: with(
                &[HUNDREDTH, SCATTERED],
                small(SMALL_BASE + 10, SMALL_BASE + 30),
            ),
        },
        Principal {
            name: "10%",
            credential: with(&[TENTH, SCATTERED], small(SMALL_BASE + 30, SMALL_BASE + 50)),
        },
        Principal {
            name: "50%",
            credential: with(&[HALF, HUNDREDTH], small(SMALL_BASE + 50, SMALL_BASE + 70)),
        },
        Principal {
            name: "95%",
            credential: with(&[MOST, SCATTERED], small(SMALL_BASE + 70, SMALL_TERMS)),
        },
    ]
}

// -------------------------------------------------------------------------------------------
// The fixture
// -------------------------------------------------------------------------------------------

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..ENTITIES).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
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

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for entity in 0..ENTITIES {
        for term in terms_of(entity) {
            entities.push(entity);
            terms.push(term);
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
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// Build the fixture bundle. Its own writers rather than `common`'s, because the whole subject
/// here is an access relation `common`'s two terms cannot express.
fn build_fixture(out: &Path, points: &Path, pairs: &Path) {
    write_points(points);
    write_pairs(pairs);
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: VIEW.to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
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
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    build(&args).expect("the fixture builds");
}

/// θ saturated and the caps above what any tile holds, for `common::config`'s reason: what these
/// cases assert is which rows a projection holds, and a live density rule would put a second
/// arithmetic between the projection and every assertion.
fn route_config() -> EngineConfig {
    EngineConfig {
        theta_target_marks: ENTITIES * 2,
        max_k: 500,
        k_max_marks: 500,
        flush_max_age_secs: 3600,
        flush_max_items: usize::MAX,
        ..config_uncapped()
    }
}

struct Fixture {
    tmp: tempfile::TempDir,
    root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Fixture { tmp, root }
}

impl Fixture {
    /// A reader — no write executor, so nothing publishes underneath a comparison.
    fn reader(&self, name: &str) -> Engine {
        Engine::open(
            &self.root,
            &self.tmp.path().join(format!("cache-{name}")),
            &self.tmp.path().join(format!("{name}.log")),
            tessera_plugin::Passthrough::new(),
            route_config(),
        )
        .expect("the engine opens")
    }

    /// A writer, with the executor started.
    fn writer(&self, name: &str) -> Engine {
        let mut engine = self.reader(name);
        engine
            .start_write_executor(64)
            .expect("the executor starts once");
        engine
    }
}

/// Authorise, waiting out a concurrent build of the same credential's fragment.
///
/// D-G (lifecycle §3.3) does not block a second caller on an in-flight fragment build: the cache
/// answers `FragmentBuilding` at once and the caller retries. The background refresh rebuilds a
/// resident session's fragment after a publication, and every case here authorises the same
/// credential several times over while the executor runs, so that answer is one a case can be
/// given. A client retries; so does this, and any other error is the failure it looks like.
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

/// Rewrite every `SEGMENTS-<n>.json` under `root` to name no term-image extent.
///
/// A view whose segments manifest names no extent for it has no image table, which is the state
/// `open_bundle` reaches without reading a file (`read.rs::open_term_images`). It is what an
/// ingest-only deployment carries until its first fold. The image files stay on disk and stay in
/// the digest maps, so the bundle verifies exactly as it did.
fn drop_term_image_extents(root: &Path) {
    fn walk(dir: &Path, found: &mut usize) {
        for entry in std::fs::read_dir(dir).expect("the bundle is readable") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, found);
                continue;
            }
            let is_segments = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("SEGMENTS-") && name.ends_with(".json"));
            if !is_segments {
                continue;
            }
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).expect("the manifest is readable"))
                    .expect("the manifest is JSON");
            let extents = manifest
                .get_mut("term_image_extents")
                .expect("a segments manifest carries the field");
            assert!(
                !extents.as_array().expect("an array").is_empty(),
                "{path:?} named no term-image extent before it was stripped"
            );
            *extents = serde_json::Value::Array(Vec::new());
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&manifest).expect("the manifest serialises"),
            )
            .expect("the manifest is writable");
            *found += 1;
        }
    }

    let mut found = 0;
    walk(root, &mut found);
    assert!(found > 0, "no segments manifest was found under {root:?}");
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn whole_extent() -> ViewportRequest<'static> {
    ViewportRequest::new(VIEW, 2, [0.0, 0.0, 1000.0, 1000.0], 500)
}

// -------------------------------------------------------------------------------------------
// The comparison every case runs
// -------------------------------------------------------------------------------------------

/// Which route ran, read off the per-route gauge across one build.
fn route_taken(before: [u64; 4], after: [u64; 4]) -> ProjectionRoute {
    let risen: Vec<ProjectionRoute> = ProjectionRoute::ALL
        .into_iter()
        .filter(|route| after[route.index()] > before[route.index()])
        .collect();
    assert_eq!(
        risen.len(),
        1,
        "exactly one route must have run for one build: {before:?} -> {after:?}"
    );
    risen[0]
}

/// **The whole property, for one principal**: the projection each of the three routes builds is
/// the same set of rows as `RowSpace::project` over the same fragment, byte for byte as `Portable`
/// bytes, and the viewport each of them serves is the same response.
///
/// The `None` arm is the route the chooser picks, which is the one a deployment takes; the two
/// forced arms are what make the equality a comparison between routes rather than between a route
/// and itself. Each arm authorises its own session, because a row projection is cached per token
/// and a second arm on one token would be served the first arm's answer.
///
/// Returns the route the chooser picked, for the report a case makes about its principals.
fn routes_agree_for(engine: &Engine, principal: &Principal, at: &str) -> ProjectionRoute {
    let mut chosen = None;
    let mut responses = Vec::new();
    for force in [
        None,
        Some(ProjectionRoute::Walk),
        Some(ProjectionRoute::Split),
        Some(ProjectionRoute::Complement),
    ] {
        engine.force_projection_route_for_test(force);
        let session = authorise(engine, &principal.credential);
        let before = engine.projection_builds_by_route();
        let rows = engine
            .session_projection_rows_for_test(&session, VIEW)
            .expect("the projection builds");
        let after = engine.projection_builds_by_route();
        let took = route_taken(before, after);

        let walked = engine
            .session_walk_rows_for_test(&session, VIEW)
            .expect("the reference walk runs");
        assert_eq!(
            rows.cardinality(),
            walked.cardinality(),
            "{at}: principal {} by {took:?} holds {} rows where the walk holds {}",
            principal.name,
            rows.cardinality(),
            walked.cardinality()
        );
        assert!(
            rows == walked,
            "{at}: principal {} by {took:?} projected a different set from the walk — a row \
             either side of that difference is a row the grant does not decide",
            principal.name
        );
        assert_eq!(
            rows.serialize::<Portable>(),
            walked.serialize::<Portable>(),
            "{at}: principal {} by {took:?} serialises differently from the walk, so the two \
             bitmaps are equal as sets and not as values",
            principal.name
        );

        match force {
            None => chosen = Some(took),
            Some(wanted) => assert_eq!(
                took, wanted,
                "{at}: principal {} was forced onto {wanted:?} and took {took:?} — a forced \
                 split with nothing to union falls back to the walk, and this case would \
                 otherwise pass having compared the walk with itself",
                principal.name
            ),
        }

        responses.push((
            took,
            engine
                .viewport(&session, whole_extent())
                .expect("the viewport serves"),
        ));
    }
    engine.force_projection_route_for_test(None);

    let (first_route, first) = &responses[0];
    for (route, response) in &responses[1..] {
        assert!(
            response == first,
            "{at}: principal {} was served a different viewport by {route:?} than by \
             {first_route:?}",
            principal.name
        );
    }
    chosen.expect("the chosen arm ran")
}

/// [`routes_agree_for`] over every principal, reporting the chosen route for each.
fn routes_agree(engine: &Engine, at: &str) -> Vec<(&'static str, ProjectionRoute)> {
    principals()
        .iter()
        .map(|principal| (principal.name, routes_agree_for(engine, principal, at)))
        .collect()
}

// -------------------------------------------------------------------------------------------
// The fixture's own premise
// -------------------------------------------------------------------------------------------

/// **Which of the fixture's terms have an image, read from the table rather than assumed.**
///
/// The keep rule is rows per Roaring container, so which terms are kept is a fact about where a
/// term's entities land in Morton order and not about how many it has. Every case below needs each
/// principal to hold at least one kept term (or the forced split falls back to the walk) and at
/// least one unkept term (or the residual is empty and the split's second half is never run). This
/// is where that premise is checked, once, so a case that fails elsewhere is not first suspected
/// of a fixture that drifted.
#[test]
fn the_fixtures_terms_are_classified_as_every_other_case_assumes() {
    let fixture = fixture();
    let engine = fixture.reader("premise");
    let generation = engine.generation();
    let view = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(VIEW))
        .expect("the fixture has one view");
    let images = view
        .term_images
        .as_ref()
        .expect("a build writes this view's term images");

    let kept = |term: u32| -> bool {
        let ids = engine.resolve_terms(&[label(term)]);
        let id = *ids
            .first()
            .unwrap_or_else(|| panic!("term {term} interned"));
        images
            .entry(id)
            .unwrap_or_else(|| panic!("term {term} is inside the table"))
            .kept()
    };

    for term in [HALF, ALMOST_ALL, MOST, TENTH, HUNDREDTH, THOUSANDTH] {
        assert!(
            kept(term),
            "term {term} must have an image, or the principals holding it never take the split"
        );
    }
    assert!(
        !kept(SCATTERED),
        "term {SCATTERED} must have no image, or the residual walk is never given a term of any \
         size"
    );
    for term in [SMALL_BASE, SMALL_BASE + 29, SMALL_TERMS + SMALL_BASE - 1] {
        assert!(
            !kept(term),
            "term {term} carries thirty entities or fewer and cannot be projected at all"
        );
    }
}

// -------------------------------------------------------------------------------------------
// The cases
// -------------------------------------------------------------------------------------------

/// **The built bundle**: five principals from a thousandth of the corpus to almost all of it, each
/// served the same rows and the same viewport by all three routes.
#[test]
fn every_route_builds_the_same_projection_over_a_built_bundle() {
    let fixture = fixture();
    let engine = fixture.reader("built");
    let chosen = routes_agree(&engine, "a built bundle");
    eprintln!("chosen routes over a built bundle: {chosen:?}");
    assert!(
        chosen
            .iter()
            .any(|(_, route)| *route == ProjectionRoute::Split),
        "no principal here chose the split, so the chosen arm of every comparison above was the \
         walk and the chooser is untested: {chosen:?}"
    );
}

/// **A principal holding almost the whole corpus is sent to the complement by the chooser**,
/// unprompted, which is the case a deployment gets.
///
/// Every other case forces the route it compares; this one reads the gauge on the chosen arm. The
/// grant is `ALMOST_ALL` and `SCATTERED`, which leaves under two hundred entities of the corpus
/// outside it, so the complement walks those where the walk would cross two hundred thousand and
/// the split would union an image covering almost every container of row space. The grant is not
/// the whole domain, because `SCATTERED` does not recover every entity `ALMOST_ALL` omits, so the
/// whole-domain answer does not take the case before the chooser sees it.
#[test]
fn a_principal_holding_almost_everything_is_sent_to_the_complement_by_the_chooser() {
    let fixture = fixture();
    let engine = fixture.reader("complement");
    let principal = Principal {
        name: "99.9%",
        credential: credential(&[ALMOST_ALL, SCATTERED]),
    };
    let chosen = routes_agree_for(&engine, &principal, "a built bundle");
    assert_eq!(
        chosen,
        ProjectionRoute::Complement,
        "a grant with a couple of hundred entities outside it must price the complement below \
         the walk and the split, or the route is live and unreachable"
    );
}

/// **A flush under a kept term and under an unkept term.**
///
/// The two halves a flush puts in the split route's way are different. Its rows live in an extent,
/// which no image covers and which the split unions separately; its terms land in a delta tier,
/// which no image covers either — so an entity flushed under a term that *has* an image must still
/// reach the projection through the residual, and a split that treated a kept term as wholly
/// covered by its image would lose exactly those rows.
#[test]
fn every_route_agrees_after_a_flush_under_a_kept_and_an_unkept_term() {
    let fixture = fixture();
    let engine = fixture.writer("flush");
    routes_agree(&engine, "before the flush");

    for (n, term) in [MOST, SCATTERED, HALF, SMALL_BASE].into_iter().enumerate() {
        ingest(&engine, &format!("flushed-{n}"), &[term], 100.0 + n as f64);
    }
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });

    let chosen = routes_agree(&engine, "after a flush");
    eprintln!("chosen routes after a flush: {chosen:?}");
}

/// **A merge**, which permutes row space inside the merged span and leaves every image addressing
/// the base rows it always addressed.
#[test]
fn every_route_agrees_after_a_merge() {
    let fixture = fixture();
    let engine = fixture.writer("merge");
    engine.set_merge_for_test(false);
    for segment in 0..4u32 {
        for item in 0..4u32 {
            ingest(
                &engine,
                &format!("merged-{segment}-{item}"),
                &[MOST, SCATTERED],
                (item * 4 + segment) as f64 * 20.0,
            );
        }
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("a flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
    }
    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });

    let chosen = routes_agree(&engine, "after a merge");
    eprintln!("chosen routes after a merge: {chosen:?}");
}

/// **An accepted delete and a suppression.**
///
/// Both are overlay state and neither reaches an image: a suppressed entity stays in its term's
/// posting and in its image, exactly as it stays in the fragment, and a deleted one leaves them
/// only at the fold that removes its rows (`write-path.md` §5.4). What must hold is that the
/// composed answer is the same whichever route built the projection underneath it, and that both
/// items are absent from every one of them.
#[test]
fn every_route_agrees_across_a_delete_and_a_suppression() {
    let fixture = fixture();
    let engine = fixture.writer("deny");

    let deleted = EntityId::new(41);
    let suppressed = EntityId::new(52);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("the delete is accepted");
    engine
        .accept_change(suppressed, ChangeOp::Suppress)
        .expect("the suppression is accepted");

    let chosen = routes_agree(&engine, "after a delete and a suppression");
    eprintln!("chosen routes after a delete and a suppression: {chosen:?}");

    // The two items are gone from every route's answer, in entity space, which is where the
    // overlay's verdict is stated.
    for force in [
        None,
        Some(ProjectionRoute::Walk),
        Some(ProjectionRoute::Split),
        Some(ProjectionRoute::Complement),
    ] {
        engine.force_projection_route_for_test(force);
        let session = authorise(&engine, &credential(&[MOST, SCATTERED]));
        engine.viewport(&session, whole_extent()).expect("serves");
        for entity in [deleted, suppressed] {
            let id = engine
                .tessera_id_of(entity)
                .expect("the identity is computable");
            assert!(
                engine
                    .item(&session, id, None)
                    .expect("the drill-down answers")
                    .is_none(),
                "{entity:?} is still visible under {force:?}"
            );
        }
    }
    engine.force_projection_route_for_test(None);
}

/// **The background refresh's rung 3 chooses a route of its own**, and what it produces is the
/// same projection a request would have built.
///
/// Rungs 1 and 2 derive from the projection a session already holds and read no image; only rung 3
/// builds, and a compaction is what drives a resident entry down to it — the prefix moves, so
/// neither derivation is licensed. This is the one case where the route is chosen on a pool thread
/// rather than on a request thread, and the gauge is shared so that it is counted the same way.
#[test]
fn the_background_refresh_builds_by_a_chosen_route_and_equals_the_walk() {
    let fixture = fixture();
    let engine = fixture.writer("refresh");

    // A resident entry for every principal, so the pass has something to refresh.
    let sessions: Vec<_> = principals()
        .into_iter()
        .map(|principal| {
            let session = authorise(&engine, &principal.credential);
            engine.viewport(&session, whole_extent()).expect("serves");
            (principal, session)
        })
        .collect();

    let before_refreshes = engine.refreshes();
    ingest(&engine, "refreshed", &[MOST], 250.0);
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });
    wait_until("the background refresh to produce every entry", || {
        engine.refreshes() >= before_refreshes + sessions.len() as u64
    });

    // A flush leaves rung 1 available, so the pass patches rather than builds and no route is
    // chosen. What must hold is that whatever it produced is still the walk's set.
    for (principal, session) in &sessions {
        let rows = engine
            .session_projection_rows_for_test(session, VIEW)
            .expect("the refreshed entry is served");
        let walked = engine
            .session_walk_rows_for_test(session, VIEW)
            .expect("the reference walk runs");
        assert!(
            rows == walked,
            "the refreshed projection for {} is not the walk's set",
            principal.name
        );
    }

    // **Rung 3, reached by a fold.** A fold publishes a new prefix, so neither derivation is
    // licensed (`refresh::carry_for`) and the pass builds — choosing a route, on a pool thread,
    // counted into the gauge the request path counts into. A fold deliberately does not arm the
    // shed (decision 0053), but it does spawn the pass.
    let before_refreshes = engine.refreshes();
    let before_routes = engine.projection_builds_by_route();
    let before_fold = engine.write_executor_stats();
    engine.request_fold();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before_fold.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before_fold.folds {
            break;
        }
        assert!(Instant::now() < deadline, "the fold never published");
        std::thread::sleep(Duration::from_millis(10));
    }
    wait_until("the post-fold refresh to produce every entry", || {
        engine.refreshes() >= before_refreshes + sessions.len() as u64
    });
    let after_routes = engine.projection_builds_by_route();
    assert!(
        after_routes.iter().sum::<u64>()
            >= before_routes.iter().sum::<u64>() + sessions.len() as u64,
        "the pass produced an entry for every session without building one, so rung 3 did not \
         choose a route: {before_routes:?} -> {after_routes:?}"
    );

    // And what it produced is the walk's set. Read before any request-path build, which is why the
    // gauge was compared above rather than after this loop.
    for (principal, session) in &sessions {
        let rows = engine
            .session_projection_rows_for_test(session, VIEW)
            .expect("the refreshed post-fold entry is served");
        let walked = engine
            .session_walk_rows_for_test(session, VIEW)
            .expect("the reference walk runs");
        assert!(
            rows == walked,
            "the post-fold projection for {} is not the walk's set",
            principal.name
        );
    }
}

/// **A principal holding no term with an image walks, and a forced split falls back to the walk.**
///
/// Two things at once, and the second is why this is a case of its own. A session with no kept
/// term has nothing to union, so `RowProjection::new` never asks the chooser and the route is the
/// walk. Forcing the split on it must not invent one: it walks, and the gauge says `Walk`. Every
/// other case here asserts that a forced route was the route taken, so without this one the
/// fallback would be reachable only by a fixture drifting into it, which is the state a test
/// should name rather than discover.
#[test]
fn a_principal_holding_no_kept_term_walks_and_a_forced_split_falls_back_to_the_walk() {
    let fixture = fixture();
    let engine = fixture.reader("no-kept-term");
    // Small terms only: every one of them carries thirty entities or fewer, so none was projected.
    let credential = credential(&(SMALL_BASE..SMALL_BASE + 20).collect::<Vec<u32>>());

    for force in [None, Some(ProjectionRoute::Split)] {
        engine.force_projection_route_for_test(force);
        let session = authorise(&engine, &credential);
        let before = engine.projection_builds_by_route();
        let rows = engine
            .session_projection_rows_for_test(&session, VIEW)
            .expect("the projection builds");
        let after = engine.projection_builds_by_route();
        assert_eq!(
            route_taken(before, after),
            ProjectionRoute::Walk,
            "a session with no kept term has no split to take, forced or chosen"
        );
        let walked = engine
            .session_walk_rows_for_test(&session, VIEW)
            .expect("the reference walk runs");
        assert!(rows == walked, "the fallback walk lost rows");
        assert!(
            rows.cardinality() > 0,
            "the principal must hold something, or every assertion here is about an empty set"
        );
    }
    engine.force_projection_route_for_test(None);
}

/// **A view with no image table is still priced**, and a near-total principal on it is sent to the
/// complement.
///
/// A view can be served without images: an ingest-only deployment has none until its first fold,
/// and a view created while running has none until then either. There is no split to take, so what
/// is left is the walk against the complement, and a grant leaving a couple of hundred entities
/// outside it is answered by walking those. The chooser is reached through the image table, so this
/// is the case that says a missing table does not take the complement away with it.
#[test]
fn a_view_with_no_image_table_is_still_priced_against_the_complement() {
    let fixture = fixture();
    drop_term_image_extents(&fixture.root);
    let engine = fixture.reader("no-table");

    let generation = engine.generation();
    let view = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(VIEW))
        .expect("the fixture has one view");
    assert!(
        view.term_images.is_none(),
        "the segments manifest names no extent for this view, so it can carry no image table"
    );

    // A grant leaving under two hundred entities outside it.
    let session = authorise(&engine, &credential(&[ALMOST_ALL, SCATTERED]));
    let before = engine.projection_builds_by_route();
    let rows = engine
        .session_projection_rows_for_test(&session, VIEW)
        .expect("the projection builds");
    let after = engine.projection_builds_by_route();
    assert_eq!(
        route_taken(before, after),
        ProjectionRoute::Complement,
        "a near-total grant over a view with no images must still price the complement"
    );
    let walked = engine
        .session_walk_rows_for_test(&session, VIEW)
        .expect("the reference walk runs");
    assert!(
        rows == walked,
        "the complement over a view with no images is not the walk's set"
    );

    // And a narrow grant on the same view walks, which is the other side of the same pricing.
    let narrow = authorise(&engine, &credential(&[THOUSANDTH]));
    let before = engine.projection_builds_by_route();
    let rows = engine
        .session_projection_rows_for_test(&narrow, VIEW)
        .expect("the projection builds");
    let after = engine.projection_builds_by_route();
    assert_eq!(
        route_taken(before, after),
        ProjectionRoute::Walk,
        "two hundred entities are walked, not two hundred thousand"
    );
    let walked = engine
        .session_walk_rows_for_test(&narrow, VIEW)
        .expect("the reference walk runs");
    assert!(rows == walked, "the walk over a narrow grant lost rows");
    assert!(
        rows.cardinality() > 0,
        "the narrow grant must hold something"
    );
}

/// **A view created while the engine runs has no images at all**, and every principal of it is
/// served by the walk with the same rows the walk produces.
///
/// A view's images are written by the build that created it and by each fold; one created at
/// runtime has had neither, so `ViewData::term_images` is `None` for it and
/// `RowProjection::new` must reach the walk without the chooser ever being asked.
#[test]
fn a_view_created_while_running_has_no_images_and_is_served_by_the_walk() {
    let fixture = fixture();
    let engine = fixture.writer("runtime-view");
    let runtime_view = "runtime";
    engine
        .create_plain_view(tessera_engine::PlainViewDeclaration {
            name: runtime_view.to_string(),
            title: None,
            projection: "none".to_string(),
            frame: tessera_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
        })
        .expect("the view is created");

    for n in 0..64u32 {
        let descriptors = vec![label(MOST), label(SCATTERED)];
        engine
            .accept_ingest(
                vec![UnallocatedRow {
                    external_id: Some(format!("runtime-{n}").into_bytes()),
                    view: runtime_view.to_string(),
                    join: None,
                    x: (n * 13 % 1000) as f64,
                    y: (n * 29 % 1000) as f64,
                    scalars: Vec::new(),
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                    scoped: Vec::new(),
                }],
                format!("runtime-batch-{n}"),
                [n as u8; 32],
            )
            .expect("the ingest is accepted");
    }
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });

    let generation = engine.generation();
    let view = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(runtime_view))
        .expect("the runtime view is in the generation");
    assert!(
        view.term_images.is_none(),
        "a view created while running has had no build and no fold, so it can carry no images"
    );

    let session = authorise(&engine, &credential(&[MOST, SCATTERED]));
    let before = engine.projection_builds_by_route();
    let rows = engine
        .session_projection_rows_for_test(&session, runtime_view)
        .expect("the projection builds");
    let after = engine.projection_builds_by_route();
    assert_eq!(
        route_taken(before, after),
        ProjectionRoute::Walk,
        "a view with no images has no route but the walk"
    );
    let walked = engine
        .session_walk_rows_for_test(&session, runtime_view)
        .expect("the reference walk runs");
    assert!(rows == walked, "the walk over a runtime view lost rows");
}

/// **A fold that executes a delete**, in the engine's own live generation and in a second engine
/// that opens the published root from scratch.
///
/// A fold is the one publication that rewrites the postings, and pass 2b derives the images from
/// the postings it wrote — so a deleted entity leaves its term's posting and its image together,
/// and by no other route. Both of the fold's readers are checked: the live generation, which the
/// executor swaps in over the prefix it just wrote, and a cold `open_bundle` of the root, which is
/// what a restart gets.
#[test]
fn every_route_agrees_after_a_fold_executes_a_delete() {
    let fixture = fixture();
    let engine = fixture.writer("fold");

    let deleted = EntityId::new(41);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("the delete is accepted");

    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            break;
        }
        assert!(Instant::now() < deadline, "the fold never published");
        std::thread::sleep(Duration::from_millis(10));
    }

    let generation = engine.generation();
    let view = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(VIEW))
        .expect("the folded generation carries the view");
    assert!(
        view.term_images.is_some(),
        "the fold must write this view's images, or every route below is the walk"
    );

    let chosen = routes_agree(&engine, "the live generation after a fold");
    eprintln!("chosen routes after a fold: {chosen:?}");

    // The same root, opened cold by a second engine on its own WAL — what a restart reads.
    let cold = fixture.reader("fold-cold");
    let cold_chosen = routes_agree(&cold, "a cold open of the folded root");
    eprintln!("chosen routes after a cold open of the folded root: {cold_chosen:?}");
    assert_eq!(
        chosen, cold_chosen,
        "the live generation and a cold open of the same root chose different routes, so one of \
         them is reading a different image table"
    );
}

/// **A view created while running gains images at its first fold**, and is then served by the
/// split where it holds a kept term — the other half of
/// [`a_view_created_while_running_has_no_images_and_is_served_by_the_walk`].
#[test]
fn a_view_created_while_running_gains_images_at_its_first_fold() {
    let fixture = fixture();
    let engine = fixture.writer("runtime-view-fold");
    let runtime_view = "runtime";
    engine
        .create_plain_view(tessera_engine::PlainViewDeclaration {
            name: runtime_view.to_string(),
            title: None,
            projection: "none".to_string(),
            frame: tessera_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
        })
        .expect("the view is created");

    // Enough entities under one term that its image passes the keep rule once the fold derives it.
    for n in 0..512u32 {
        let descriptors = vec![label(MOST)];
        engine
            .accept_ingest(
                vec![UnallocatedRow {
                    external_id: Some(format!("runtime-{n}").into_bytes()),
                    view: runtime_view.to_string(),
                    join: None,
                    x: (n * 13 % 1000) as f64,
                    y: (n * 29 % 1000) as f64,
                    scalars: Vec::new(),
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                    scoped: Vec::new(),
                }],
                format!("runtime-batch-{n}"),
                [(n % 251) as u8; 32],
            )
            .expect("the ingest is accepted");
    }
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });

    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            break;
        }
        assert!(Instant::now() < deadline, "the fold never published");
        std::thread::sleep(Duration::from_millis(10));
    }

    let generation = engine.generation();
    let view = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(runtime_view))
        .expect("the folded generation carries the runtime view");
    assert!(
        view.term_images.is_some(),
        "an ingest-only view must gain its images at its first fold"
    );

    let principal = Principal {
        name: "the runtime view's principal",
        credential: credential(&[MOST]),
    };
    let session = authorise(&engine, &principal.credential);
    engine.force_projection_route_for_test(Some(ProjectionRoute::Split));
    let before_routes = engine.projection_builds_by_route();
    let rows = engine
        .session_projection_rows_for_test(&session, runtime_view)
        .expect("the projection builds");
    let after_routes = engine.projection_builds_by_route();
    assert_eq!(
        route_taken(before_routes, after_routes),
        ProjectionRoute::Split,
        "the folded runtime view holds a kept term, so a forced split must union it"
    );
    let walked = engine
        .session_walk_rows_for_test(&session, runtime_view)
        .expect("the reference walk runs");
    assert!(
        rows == walked,
        "the split over a folded runtime view is not the walk's set"
    );
    engine.force_projection_route_for_test(None);
}

// -------------------------------------------------------------------------------------------
// Ingest
// -------------------------------------------------------------------------------------------

fn ingest(engine: &Engine, external_id: &str, terms: &[u32], x: f64) {
    let descriptors: Vec<Vec<u8>> = terms.iter().copied().map(label).collect();
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: VIEW.to_string(),
        join: None,
        x,
        y: 7.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        descriptors,
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("the ingest is accepted");
}
